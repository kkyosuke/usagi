//! terminal の振る舞いを固定するテスト。

use super::*;
use usagi_core::domain::id::{DaemonGeneration, SessionId, TerminalId, WorkspaceId, WorktreeId};
use usagi_core::infrastructure::ipc::{DEFAULT_MAX_FRAME_BYTES, write_json_frame};

#[derive(Default)]
struct Writer {
    written: Vec<u8>,
    failure: Option<usize>,
    /// Every geometry the registry actually applied to the PTY, in order.
    resized: Vec<Geometry>,
    /// Whether the PTY refuses the next size it is given.
    resize_failure: bool,
}
impl PtyWriter for Writer {
    fn resize(&mut self, _: &TerminalRef, geometry: Geometry) -> Result<(), PtyWriteError> {
        if self.resize_failure {
            return Err(PtyWriteError { applied_prefix: 0 });
        }
        self.resized.push(geometry);
        Ok(())
    }
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
        self.written.extend_from_slice(bytes);
        self.failure.map_or(Ok(()), |applied_prefix| {
            Err(PtyWriteError { applied_prefix })
        })
    }
}
fn reference() -> TerminalRef {
    TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    }
}
fn registry(reference: TerminalRef) -> TerminalRegistry {
    let mut registry = TerminalRegistry::new(4, 2);
    registry
        .register(reference, Geometry { cols: 80, rows: 24 })
        .unwrap();
    registry
}
fn input(
    subscription: u64,
    connection: ConnectionId,
    client: ClientId,
    request: RequestId,
    input_seq: u64,
) -> InputRequest {
    InputRequest {
        subscription,
        connection,
        client,
        request,
        input_seq,
        operation: None,
    }
}

/// The same input carrying a producer-issued durable operation identity.
fn durable_input(
    subscription: u64,
    connection: ConnectionId,
    client: ClientId,
    request: RequestId,
    input_seq: u64,
    operation: OperationId,
) -> InputRequest {
    InputRequest {
        operation: Some(operation),
        ..input(subscription, connection, client, request, input_seq)
    }
}

/// The screen a client reconstructs from a revision 2 frame.
fn restored(frame: &SnapshotFrame) -> VtScreen {
    let checkpoint = frame
        .content
        .screen()
        .expect("a revision 2 frame carries a screen checkpoint");
    VtScreen::from_checkpoint(checkpoint).expect("the daemon emits a decodable checkpoint")
}

#[test]
fn revision_2_snapshot_reconstructs_a_screen_a_trimmed_raw_tail_cannot() {
    let r = reference();
    // A four byte journal keeps almost nothing, so the raw tail starts in the
    // middle of an escape sequence — the regression this replaces.
    let mut registry = TerminalRegistry::new(4, 2);
    registry
        .register(r.clone(), Geometry { cols: 12, rows: 3 })
        .unwrap();
    let stream: Vec<&[u8]> = vec![
        b"\x1b[1;31mred\x1b[0m plain\r\n",
        "\u{65e5}\u{672c}".as_bytes(),
        b"\x1b[?1049halt\r\nscreen\x1b[?1049l",
        b"tail\x1b[2;3Hx\x1b[1;38;5;208m",
    ];
    for chunk in &stream {
        registry.append_output(&r, (*chunk).to_vec()).unwrap();
    }

    let attached = registry.attach(&r, ConnectionId::new()).unwrap();
    let checkpoint = attached
        .clone()
        .into_frame(SnapshotWire::ScreenCheckpoint)
        .snapshot;
    // A checkpoint is complete at `output_offset`, so it carries no tail.
    assert_eq!(checkpoint.base_offset, checkpoint.output_offset);
    assert_eq!(checkpoint.geometry, Geometry { cols: 12, rows: 3 });

    // The reconstructed screen equals a reference parser fed every byte,
    // including the cursor move, the styles and the alternate excursion the
    // four byte tail cannot express.
    let mut reference_screen = VtScreen::new(3, 12);
    for chunk in &stream {
        reference_screen.advance(chunk);
    }
    let rebuilt = restored(&checkpoint);
    assert_eq!(rebuilt.cells(), reference_screen.cells());
    assert_eq!(
        rebuilt.cells_with_scrollback(),
        reference_screen.cells_with_scrollback()
    );
    assert_eq!(rebuilt.cursor(), reference_screen.cursor());
    assert_eq!(rebuilt.cursor_style(), reference_screen.cursor_style());

    // A revision 1 client keeps the legacy raw tail contract unchanged.
    let raw = attached.into_frame(SnapshotWire::RawTail).snapshot;
    assert_eq!(
        raw.content,
        SnapshotContent::RawTail {
            replay: b"208m".to_vec()
        }
    );
    assert_eq!(raw.content.replay(), Some(&b"208m"[..]));
    assert_eq!(raw.base_offset + 4, raw.output_offset);
    // Each payload exposes only its own shape.
    assert!(raw.content.screen().is_none());
    assert!(checkpoint.content.replay().is_none());

    // Wire selection follows the negotiated revision.
    assert_eq!(SnapshotWire::for_revision(0), SnapshotWire::RawTail);
    assert_eq!(SnapshotWire::for_revision(1), SnapshotWire::RawTail);
    assert_eq!(
        SnapshotWire::for_revision(2),
        SnapshotWire::ScreenCheckpoint
    );
    assert_eq!(SnapshotWire::default(), SnapshotWire::RawTail);
}

#[test]
fn checkpoint_and_resume_suffix_reconstruct_the_authoritative_screen() {
    let r = reference();
    let mut registry = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 2);
    registry
        .register(r.clone(), Geometry { cols: 10, rows: 4 })
        .unwrap();
    registry
        .append_output(&r, b"first\r\nsecond\x1b[1m".to_vec())
        .unwrap();

    let frame = registry
        .snapshot(&r)
        .unwrap()
        .into_frame(SnapshotWire::ScreenCheckpoint);
    let mut client = restored(&frame);

    // Output produced after the checkpoint arrives as a contiguous raw
    // suffix; the restored parser continues the interrupted sequence.
    registry
        .append_output(&r, b"bold\r\nthird".to_vec())
        .unwrap();
    for segment in registry.replay_from(&r, frame.output_offset, None).unwrap() {
        client.advance(&segment.data);
    }
    let authority = registry
        .snapshot(&r)
        .unwrap()
        .into_frame(SnapshotWire::ScreenCheckpoint);
    assert_eq!(client, restored(&authority));
}

#[test]
fn resize_fences_revision_and_geometry_around_checkpoint_capture() {
    let r = reference();
    let mut registry = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 2);
    registry
        .register(r.clone(), Geometry { cols: 12, rows: 3 })
        .unwrap();
    registry
        .append_output(&r, b"alpha\r\nbeta\r\ngamma".to_vec())
        .unwrap();

    // Captured before the resize.
    let before = registry
        .snapshot(&r)
        .unwrap()
        .into_frame(SnapshotWire::ScreenCheckpoint);

    // The resize holds the registry's exclusive borrow across preflight, PTY
    // effect and commit, and returns the post-resize view.
    let after = registry
        .resize(
            &r,
            Geometry { cols: 6, rows: 2 },
            None,
            &mut Writer::default(),
        )
        .unwrap()
        .into_frame(SnapshotWire::ScreenCheckpoint);
    assert_eq!(after.revision, before.revision + 1);
    assert_eq!(after.geometry, Geometry { cols: 6, rows: 2 });

    // No frame ever mixes geometries: the envelope and the screen it carries
    // always agree, so a client detects the fence by comparing either one.
    for frame in [&before, &after] {
        let screen = frame.content.screen().expect("checkpoint frame");
        assert_eq!(u32::from(frame.geometry.rows), screen.geometry.rows);
        assert_eq!(u32::from(frame.geometry.cols), screen.geometry.cols);
    }

    // A suffix applied to the pre-resize checkpoint diverges from the
    // authority, which is exactly why the client must retry on the revision
    // or geometry mismatch instead of merging the two states.
    registry.append_output(&r, b"\r\nafter".to_vec()).unwrap();
    let mut stale = restored(&before);
    for segment in registry
        .replay_from(&r, before.output_offset, None)
        .unwrap()
    {
        stale.advance(&segment.data);
    }
    let authority = registry
        .snapshot(&r)
        .unwrap()
        .into_frame(SnapshotWire::ScreenCheckpoint);
    assert_ne!(stale, restored(&authority));
    assert!(authority.revision > before.revision);

    // Re-attaching after the fence converges: the fresh checkpoint plus its
    // own suffix reproduces the authority at the new geometry.
    let fresh = registry
        .snapshot(&r)
        .unwrap()
        .into_frame(SnapshotWire::ScreenCheckpoint);
    assert_eq!(restored(&fresh), restored(&authority));
    assert_eq!(fresh.geometry, Geometry { cols: 6, rows: 2 });
}

#[test]
fn the_output_window_reports_offsets_without_capturing_a_screen() {
    let r = reference();
    // A zero checkpoint budget makes any screen capture fail closed, so a
    // window that still succeeds cannot have taken one.
    let mut registry = TerminalRegistry::new(4, 2).with_checkpoint_bytes_limit(0);
    registry
        .register(r.clone(), Geometry { cols: 8, rows: 2 })
        .unwrap();
    registry.append_output(&r, b"abcdef".to_vec()).unwrap();

    assert_eq!(
        registry.snapshot(&r).unwrap_err(),
        RegistryError::CheckpointUnavailable
    );
    // The journal retains 4 of the 6 accepted bytes, so the window starts at
    // 2 and the terminal has accepted 6.
    assert_eq!(
        registry.output_window(&r).unwrap(),
        OutputWindow {
            base_offset: 2,
            output_offset: 6,
            exited: None,
        }
    );

    registry.exited(&r, 3).unwrap();
    assert_eq!(registry.output_window(&r).unwrap().exited, Some(3));
    assert_eq!(
        registry.output_window(&reference()).unwrap_err(),
        RegistryError::StaleTarget
    );
}

/// Rows a checkpoint's primary buffer retains (visible grid plus history).
fn retained_rows(frame: &SnapshotFrame) -> usize {
    let screen = frame.content.screen().expect("checkpoint frame");
    screen.primary.grid.len() + screen.primary.scrollback.len()
}
/// Registers `count` terminals and feeds each of them `lines` rows.
fn fed_terminals(
    registry: &mut TerminalRegistry,
    geometry: Geometry,
    count: usize,
    lines: usize,
) -> Vec<TerminalRef> {
    let terminals: Vec<TerminalRef> = (0..count).map(|_| reference()).collect();
    for terminal in &terminals {
        registry.register(terminal.clone(), geometry).unwrap();
    }
    for line in 0..lines {
        for terminal in &terminals {
            registry
                .append_output(terminal, format!("line{line}\r\n").into_bytes())
                .unwrap();
        }
    }
    terminals
}

#[test]
fn a_screen_retains_at_most_its_per_terminal_cell_budget() {
    let geometry = Geometry { cols: 8, rows: 2 };
    // Four rows of retention: the two visible rows plus two of history.
    let budget = 4 * usize::from(geometry.cols);
    let before = output_pipeline_counters();
    // The process ceiling is lifted so this asserts the per-terminal bound
    // alone, independent of the screens other tests hold in this process.
    let mut registry = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 2)
        .with_screen_cell_budgets(budget, usize::MAX);
    let terminals = fed_terminals(&mut registry, geometry, 2, 64);

    for terminal in &terminals {
        let frame = registry
            .snapshot(terminal)
            .unwrap()
            .into_frame(SnapshotWire::ScreenCheckpoint);
        let retained = retained_rows(&frame) * usize::from(geometry.cols);
        assert_eq!(retained, budget, "the budget is used, and not exceeded");
        // History survives: the peak is not paid for by dropping everything.
        assert!(retained_rows(&frame) > usize::from(geometry.rows));
    }
    assert!(output_pipeline_counters().screen_trimmed_rows > before.screen_trimmed_rows);
}

#[test]
fn every_screen_is_trimmed_into_the_process_aggregate_ceiling() {
    let geometry = Geometry { cols: 8, rows: 2 };
    // A ceiling this small cannot be shared by three terminals, so each one
    // is trimmed as it grows however much the others already retain.
    let ceiling = 6 * usize::from(geometry.cols);
    let mut registry = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 2)
        .with_screen_cell_budgets(usize::MAX, ceiling);
    let terminals = fed_terminals(&mut registry, geometry, 3, 64);

    let mut total = 0;
    for terminal in &terminals {
        let frame = registry
            .snapshot(terminal)
            .unwrap()
            .into_frame(SnapshotWire::ScreenCheckpoint);
        let retained = retained_rows(&frame) * usize::from(geometry.cols);
        assert!(
            retained <= ceiling,
            "one terminal retained {retained} cells above the {ceiling} cell ceiling"
        );
        total += retained;
    }
    // The visible grids are the floor the ceiling cannot reclaim.
    let floor = terminals.len() * usize::from(geometry.rows) * usize::from(geometry.cols);
    assert!(total <= ceiling + floor);
}

#[test]
fn oversized_checkpoints_trim_history_and_then_fail_closed() {
    let r = reference();
    // A budget far below a full checkpoint forces payload trimming.
    let mut registry =
        TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 2).with_checkpoint_bytes_limit(2048);
    registry
        .register(r.clone(), Geometry { cols: 16, rows: 2 })
        .unwrap();
    for line in 0..64 {
        registry
            .append_output(&r, format!("history line {line}\r\n").into_bytes())
            .unwrap();
    }
    // A full-screen application then owns the alternate buffer, so the
    // checkpoint carries history in both buffers and both are trimmed.
    registry.append_output(&r, b"\x1b[?1049h".to_vec()).unwrap();
    for line in 0..64 {
        registry
            .append_output(&r, format!("alternate line {line}\r\n").into_bytes())
            .unwrap();
    }
    let before = output_pipeline_counters();
    let frame = registry
        .snapshot(&r)
        .unwrap()
        .into_frame(SnapshotWire::ScreenCheckpoint);
    let screen = frame.content.screen().expect("checkpoint frame");
    let authority = registry.entry(&r).unwrap().screen.checkpoint();
    assert!(serde_json::to_vec(screen).unwrap().len() <= 2048);
    assert_eq!(
        screen.primary.scrollback_origin + u64::try_from(screen.primary.scrollback.len()).unwrap(),
        authority.primary.scrollback_origin
            + u64::try_from(authority.primary.scrollback.len()).unwrap(),
        "payload trimming must preserve the primary logical tail"
    );
    let screen_alternate = screen.alternate.as_ref().expect("alternate payload");
    let authority_alternate = authority.alternate.as_ref().expect("alternate authority");
    assert_eq!(
        screen_alternate.scrollback_origin
            + u64::try_from(screen_alternate.scrollback.len()).unwrap(),
        authority_alternate.scrollback_origin
            + u64::try_from(authority_alternate.scrollback.len()).unwrap(),
        "payload trimming must preserve the alternate logical tail"
    );
    let trimmed_once = output_pipeline_counters().checkpoint_trimmed_rows;
    assert!(trimmed_once > before.checkpoint_trimmed_rows);

    // Only the payload was trimmed: the authoritative screen keeps its
    // history, so the next capture has to trim the same rows again.
    registry.snapshot(&r).unwrap();
    assert!(output_pipeline_counters().checkpoint_trimmed_rows > trimmed_once);

    // A budget that cannot hold even the visible grid fails closed, and the
    // failed attach leaves no subscription behind.
    let mut tiny =
        TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 2).with_checkpoint_bytes_limit(8);
    tiny.register(r.clone(), Geometry { cols: 8, rows: 2 })
        .unwrap();
    let connection = ConnectionId::new();
    assert_eq!(
        tiny.attach(&r, connection),
        Err(RegistryError::CheckpointUnavailable)
    );
    assert_eq!(
        tiny.detach(&r, 1, connection, &mut Writer::default()),
        Err(RegistryError::UnknownSubscription)
    );
    assert_eq!(tiny.snapshot(&r), Err(RegistryError::CheckpointUnavailable));
    assert_eq!(
        tiny.resize(
            &r,
            Geometry { cols: 9, rows: 2 },
            None,
            &mut Writer::default()
        ),
        Err(RegistryError::CheckpointUnavailable)
    );
}

#[test]
fn a_geometry_beyond_the_screen_bounds_is_clamped_by_the_authority() {
    // The IPC boundary rejects such a geometry; the authority still clamps so
    // a forged dimension cannot drive an unbounded grid allocation.
    assert_eq!(
        screen_dimensions(Geometry {
            cols: u16::MAX,
            rows: u16::MAX
        }),
        (ROWS_MAX as usize, COLS_MAX as usize)
    );
    assert_eq!(screen_dimensions(Geometry { cols: 0, rows: 0 }), (1, 1));
    assert_eq!(screen_dimensions(Geometry { cols: 80, rows: 24 }), (24, 80));
}

#[test]
fn visible_grids_are_refused_before_allocation_or_pty_resize() {
    let terminal = reference();
    let mut registry = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 2)
        .with_screen_cell_budgets(16, usize::MAX);
    // This registry accounts for itself (the budget override already says
    // so), and so does the assertion: the process counter moves whenever a
    // test running beside this one registers a screen.
    assert_eq!(registry.retained_screen_cells(), 0);

    assert_eq!(
        registry.register(terminal.clone(), Geometry { cols: 5, rows: 4 }),
        Err(RegistryError::ScreenBudgetExceeded)
    );
    assert_eq!(registry.retained_screen_cells(), 0);

    let initial = Geometry { cols: 4, rows: 4 };
    registry.register(terminal.clone(), initial).unwrap();
    let connection = ConnectionId::new();
    let client = ClientId::new();
    registry
        .attach_for_client(&terminal, connection, client, None, &mut Writer::default())
        .unwrap();
    let mut writer = Writer::default();
    let oversized = Geometry { cols: 5, rows: 4 };
    assert_eq!(
        registry.resize(&terminal, oversized, Some(&client), &mut writer),
        Err(RegistryError::ScreenBudgetExceeded)
    );
    // A rejected resize still records the client's requested viewport. A
    // later attach reconciles that claim but must refuse it before asking
    // the PTY to allocate the same oversized grid.
    registry
        .attach_for_client(&terminal, connection, client, Some(oversized), &mut writer)
        .unwrap();
    assert!(writer.resized.is_empty());
    assert_eq!(registry.snapshot(&terminal).unwrap().geometry, initial);
    // The refused resize left the accepted grid in place, not the one it
    // asked for.
    assert_eq!(
        registry.retained_screen_cells(),
        counted(usize::from(initial.cols) * usize::from(initial.rows))
    );
}

#[test]
fn registration_refuses_a_visible_grid_beyond_the_remaining_process_budget() {
    let ceiling = 32;
    let mut registry = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 2)
        .with_screen_cell_budgets(usize::MAX, ceiling);
    for _ in 0..2 {
        registry
            .register(reference(), Geometry { cols: 4, rows: 4 })
            .unwrap();
    }
    assert_eq!(
        registry.register(reference(), Geometry { cols: 1, rows: 1 }),
        Err(RegistryError::ScreenBudgetExceeded)
    );
}

#[test]
fn registration_reclaims_history_before_refusing_a_grid() {
    // Busy screens fill the whole ceiling with history and nothing handed it
    // back, so every later launch was refused until the daemon restarted.
    let ceiling = 96;
    let mut registry = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 2)
        .with_screen_cell_budgets(usize::MAX, ceiling);
    let exited = reference();
    let live = reference();
    let grid = Geometry { cols: 4, rows: 4 };
    let lines = |count: usize| {
        (0..count)
            .flat_map(|line| format!("{line}\r\n").into_bytes())
            .collect::<Vec<_>>()
    };
    registry.register(exited.clone(), grid).unwrap();
    registry.register(live.clone(), grid).unwrap();
    registry.append_output(&live, lines(8)).unwrap();
    // The later screen takes everything the ceiling still leaves.
    registry.append_output(&exited, lines(40)).unwrap();
    registry.exited(&exited, 0).unwrap();
    let held = |registry: &TerminalRegistry, terminal: &TerminalRef| {
        registry.entries[&key(terminal)].screen_cells
    };
    let (exited_before, live_before) = (held(&registry, &exited), held(&registry, &live));
    assert!(exited_before > 16 && live_before > 16);
    assert_eq!(registry.retained_screen_cells(), counted(ceiling));

    // The finished screen pays first.
    registry.register(reference(), grid).unwrap();
    assert!(held(&registry, &exited) < exited_before);
    assert_eq!(held(&registry, &live), live_before);
    // Once it is down to its grid the live screen pays, and when only
    // visible grids are left the registration is refused.
    let refused = loop {
        if let Err(error) = registry.register(reference(), grid) {
            break error;
        }
        assert!(registry.retained_screen_cells() <= counted(ceiling));
    };
    assert_eq!(refused, RegistryError::ScreenBudgetExceeded);
    assert_eq!(held(&registry, &exited), 16);
    assert_eq!(held(&registry, &live), 16);
}

#[test]
fn retention_reads_what_a_terminal_holds_and_forgets_it_exactly_once() {
    let r = reference();
    let mut registry = registry(r.clone());
    assert_eq!(registry.retained_bytes(&r), 0);
    assert!(!registry.is_attached(&r));
    registry.append_output(&r, b"abc".to_vec()).unwrap();
    assert_eq!(registry.retained_bytes(&r), 3);

    let connection = ConnectionId::new();
    let attached = registry.attach(&r, connection).unwrap();
    assert!(registry.is_attached(&r));
    registry
        .detach(
            &r,
            attached.subscription,
            connection,
            &mut Writer::default(),
        )
        .unwrap();
    assert!(!registry.is_attached(&r));

    // A stale identity reads nothing and forgets nothing.
    let mut stale = r.clone();
    stale.worktree_id = WorktreeId::new();
    assert_eq!(registry.retained_bytes(&stale), 0);
    assert!(!registry.is_attached(&stale));
    assert!(!registry.forget(&stale));
    assert_eq!(registry.retained_bytes(&r), 3);

    // Forgetting releases the journal and the screen; a retry is a no-op.
    assert!(registry.forget(&r));
    assert!(!registry.forget(&r));
    assert_eq!(registry.snapshot(&r), Err(RegistryError::StaleTarget));
}

#[test]
fn exit_status_is_readable_without_capturing_a_screen() {
    let r = reference();
    let mut registry = registry(r.clone());
    assert_eq!(registry.exit_status(&r), Ok(None));
    registry.exited(&r, 3).unwrap();
    assert_eq!(registry.exit_status(&r), Ok(Some(3)));
    let mut stale = r;
    stale.worktree_id = WorktreeId::new();
    assert_eq!(
        registry.exit_status(&stale),
        Err(RegistryError::StaleTarget)
    );
}

#[test]
fn attach_is_atomic_and_disconnect_keeps_terminal() {
    let r = reference();
    let mut registry = registry(r.clone());
    let c = ConnectionId::new();
    let attached = registry.attach(&r, c).unwrap();
    assert_eq!(attached.snapshot.output_offset, 0);
    assert_eq!(
        registry.attach(&r, c).unwrap().subscription,
        attached.subscription
    );
    registry.disconnect(c, &mut Writer::default());
    assert!(registry.snapshot(&r).is_ok());
    assert_eq!(
        registry.detach(&r, attached.subscription, c, &mut Writer::default()),
        Err(RegistryError::UnknownSubscription)
    );
}

#[test]
fn live_connection_sweep_coalesces_stale_attachments_and_input_epochs() {
    let r = reference();
    let mut registry = registry(r.clone());
    let stale_connection = ConnectionId::new();
    let stale_client = ClientId::new();
    let live_connection = ConnectionId::new();
    let live_client = ClientId::new();
    let mut writer = Writer::default();
    let stale = registry
        .attach_for_client(&r, stale_connection, stale_client, None, &mut writer)
        .unwrap();
    let live = registry
        .attach_for_client(&r, live_connection, live_client, None, &mut writer)
        .unwrap();
    for (attached, connection, client, bytes) in [
        (&stale, stale_connection, stale_client, b"stale".as_slice()),
        (&live, live_connection, live_client, b"live".as_slice()),
    ] {
        registry
            .write_input(
                &r,
                input(
                    attached.subscription,
                    connection,
                    client,
                    RequestId::new(),
                    0,
                ),
                bytes,
                0,
                &mut writer,
            )
            .unwrap();
    }

    registry.retain_live_connections(&BTreeSet::from([live_connection]), &mut Writer::default());

    let reattached_live = registry
        .attach_for_client(&r, live_connection, live_client, None, &mut writer)
        .unwrap();
    assert_eq!(reattached_live.subscription, live.subscription);
    assert_eq!(reattached_live.next_input_seq, Some(1));
    let reattached_stale = registry
        .attach_for_client(&r, stale_connection, stale_client, None, &mut writer)
        .unwrap();
    assert_ne!(reattached_stale.subscription, stale.subscription);
    assert_eq!(reattached_stale.next_input_seq, Some(0));
}
#[test]
fn duplicate_registration_and_exact_detach_are_fenced() {
    let r = reference();
    let mut registry = registry(r.clone());
    assert_eq!(
        registry.register(r.clone(), Geometry { cols: 80, rows: 24 }),
        Err(RegistryError::StaleTarget)
    );
    let connection = ConnectionId::new();
    let subscription = registry.attach(&r, connection).unwrap().subscription;
    assert_eq!(
        registry.detach(&r, subscription, connection, &mut Writer::default()),
        Ok(())
    );
}
#[test]
fn output_offsets_are_contiguous_and_old_output_requires_resync() {
    let r = reference();
    let mut registry = registry(r.clone());
    assert_eq!(
        Writer::default().resize(&r, Geometry { cols: 80, rows: 24 }),
        Ok(())
    );
    assert_eq!(
        registry
            .append_output(&r, b"abc".to_vec())
            .unwrap()
            .end_offset,
        3
    );
    assert_eq!(
        registry
            .append_output(&r, b"def".to_vec())
            .unwrap()
            .start_offset,
        3
    );
    assert_eq!(
        registry.replay_from(&r, 0, None),
        Err(RegistryError::ResyncRequired)
    );
    assert_eq!(registry.replay_from(&r, 3, None).unwrap()[0].data, b"def");
    assert_eq!(registry.replay_from(&r, 4, None).unwrap()[0].data, b"ef");
    assert_eq!(
        registry.replay_from(&r, 7, None),
        Err(RegistryError::ResyncRequired)
    );
    let snapshot = registry.snapshot(&r).unwrap();
    assert_eq!(snapshot.base_offset, 2);
    assert_eq!(snapshot.output_offset, 6);
    assert_eq!(snapshot.replay, b"cdef");
}
#[test]
fn oversized_output_retains_an_exact_frame_safe_tail() {
    let r = reference();
    let mut registry = TerminalRegistry::new(usize::MAX, 1);
    registry
        .register(r.clone(), Geometry { cols: 80, rows: 24 })
        .unwrap();
    let bytes = vec![7; MAX_RETAINED_OUTPUT_BYTES + 17];
    let output = registry.append_output(&r, bytes.clone()).unwrap();
    assert_eq!(output.data, bytes);
    let snapshot = registry.snapshot(&r).unwrap();
    assert_eq!(snapshot.base_offset, 17);
    assert_eq!(snapshot.output_offset, bytes.len() as u64);
    assert_eq!(snapshot.replay.len(), MAX_RETAINED_OUTPUT_BYTES);
    assert_eq!(
        registry.replay_from(&r, 17, None).unwrap()[0].data.len(),
        MAX_RETAINED_OUTPUT_BYTES
    );
    assert_eq!(
        registry.replay_from(&r, 16, None),
        Err(RegistryError::ResyncRequired)
    );
}
#[test]
fn multi_megabyte_producers_keep_attach_and_resume_frames_bounded() {
    let counters_before = output_pipeline_counters();
    let first = reference();
    let second = reference();
    let mut registry = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 1);
    for terminal in [&first, &second] {
        registry
            .register(terminal.clone(), Geometry { cols: 80, rows: 24 })
            .unwrap();
    }
    let chunk = vec![b'x'; 4096];
    for _ in 0..300 {
        registry.append_output(&first, chunk.clone()).unwrap();
        registry.append_output(&second, chunk.clone()).unwrap();
    }

    for terminal in [&first, &second] {
        let connection = ConnectionId::new();
        let attached = registry.attach(terminal, connection).unwrap();
        for _ in 0..8 {
            let reattached = registry.attach(terminal, connection).unwrap();
            assert_eq!(reattached.subscription, attached.subscription);
            assert_eq!(
                reattached.snapshot.output_offset,
                attached.snapshot.output_offset
            );
        }
        assert_eq!(attached.snapshot.replay.len(), MAX_RETAINED_OUTPUT_BYTES);
        assert_eq!(
            attached.snapshot.base_offset + attached.snapshot.replay.len() as u64,
            attached.snapshot.output_offset
        );
        // Both negotiated payloads stay inside one frame.
        for wire in [SnapshotWire::RawTail, SnapshotWire::ScreenCheckpoint] {
            let mut frame = Vec::new();
            write_json_frame(
                &mut frame,
                &attached.clone().into_frame(wire),
                DEFAULT_MAX_FRAME_BYTES,
            )
            .unwrap();
            assert!(frame.len() < DEFAULT_MAX_FRAME_BYTES);
        }

        let cursor = attached.snapshot.base_offset + 123;
        let resumed = registry.replay_from(terminal, cursor, None).unwrap();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].start_offset, cursor);
        assert_eq!(resumed[0].end_offset, attached.snapshot.output_offset);
        let mut frame = Vec::new();
        write_json_frame(&mut frame, &resumed, DEFAULT_MAX_FRAME_BYTES).unwrap();
        assert!(frame.len() < DEFAULT_MAX_FRAME_BYTES);
    }
    let counters_after = output_pipeline_counters();
    assert!(counters_after.dropped_bytes > counters_before.dropped_bytes);
    assert!(counters_after.coalesced_bytes > counters_before.coalesced_bytes);
}
#[test]
fn input_is_acked_only_once_after_write_and_partial_is_ambiguous() {
    let r = reference();
    let mut registry = registry(r.clone());
    let connection = ConnectionId::new();
    let subscription = registry.attach(&r, connection).unwrap().subscription;
    let client = ClientId::new();
    let request = RequestId::new();
    let mut writer = Writer::default();
    assert_eq!(
        registry
            .write_input(
                &r,
                input(subscription, connection, client, request, 0),
                b"ok",
                0,
                &mut writer
            )
            .unwrap(),
        InputAck::Written
    );
    assert_eq!(
        registry
            .write_input(
                &r,
                input(subscription, connection, client, request, 0),
                b"ok",
                0,
                &mut writer
            )
            .unwrap(),
        InputAck::Cached(Box::new(InputAck::Written))
    );
    assert_eq!(writer.written, b"ok");
    let mut partial = Writer {
        failure: Some(1),
        ..Writer::default()
    };
    assert_eq!(
        registry
            .write_input(
                &r,
                input(subscription, connection, client, RequestId::new(), 1),
                b"x",
                0,
                &mut partial
            )
            .unwrap(),
        InputAck::Ambiguous { applied_prefix: 1 }
    );
    assert_eq!(
        registry.write_input(
            &r,
            input(subscription, connection, client, RequestId::new(), 3),
            b"gap",
            0,
            &mut writer
        ),
        Err(RegistryError::SequenceGap)
    );
    let mut failed = Writer {
        failure: Some(0),
        ..Writer::default()
    };
    assert_eq!(
        registry
            .write_input(
                &r,
                input(subscription, connection, client, RequestId::new(), 2),
                b"fail",
                0,
                &mut failed
            )
            .unwrap(),
        InputAck::Failed
    );
    assert_eq!(
        registry.write_input(
            &r,
            input(subscription, connection, client, request, 0),
            b"old",
            0,
            &mut writer
        ),
        Err(RegistryError::IdempotencyExpired)
    );
}
/// The core of #519: the response was produced, the connection that carried
/// it died, and the client comes back on a *new* connection with a *new*
/// subscription and a restarted epoch-local sequence. The stable operation
/// identity is the only thing that still ties the two together, so the PTY
/// must be written exactly once and both answers must be the same final.
#[test]
fn a_recorded_operation_converges_on_one_pty_write_across_connections() {
    let r = reference();
    let mut registry = registry(r.clone());
    let client = ClientId::new();
    let operation = OperationId::new();
    let mut writer = Writer::default();

    let first = ConnectionId::new();
    let subscription = registry.attach(&r, first).unwrap().subscription;
    assert_eq!(
        registry
            .write_input(
                &r,
                durable_input(subscription, first, client, RequestId::new(), 0, operation),
                b"ls\r",
                1_000,
                &mut writer,
            )
            .unwrap(),
        InputAck::Written
    );

    // The connection is gone: every attachment it owned is released.
    registry.disconnect(first, &mut Writer::default());
    let second = ConnectionId::new();
    let fresh = registry.attach(&r, second).unwrap().subscription;
    assert_ne!(fresh, subscription);

    // The epoch-local sequence restarted at zero, and the operation identity
    // still resolves the recorded final without another write.
    assert_eq!(
        registry
            .write_input(
                &r,
                durable_input(fresh, second, client, RequestId::new(), 0, operation),
                b"ls\r",
                1_100,
                &mut writer,
            )
            .unwrap(),
        InputAck::Cached(Box::new(InputAck::Written))
    );
    // The read-only query answers the same value.
    assert_eq!(
        registry
            .input_outcome(&r, client, operation, 1_100)
            .unwrap(),
        Some(InputAck::Written)
    );
    assert_eq!(writer.written, b"ls\r");
}

/// The two ledgers have deliberately different lifetimes: `input_seq` is
/// epoch-local and dies with its connection, while the operation ledger is
/// keyed by client incarnation and survives it.
///
/// Regression: making the client incarnation stable (so operations *can* be
/// resolved after a reconnect) must not make the sequence ledger outlive the
/// connection too — the client restarts `input_seq` at zero on a fresh epoch,
/// so a surviving ledger would reject its first input after every reconnect.
#[test]
fn a_fresh_connection_restarts_the_sequence_ledger_but_not_the_operation_ledger() {
    let r = reference();
    let mut registry = registry(r.clone());
    let client = ClientId::new();
    let operation = OperationId::new();
    let mut writer = Writer::default();

    let first = ConnectionId::new();
    let subscription = registry.attach(&r, first).unwrap().subscription;
    for seq in 0..3 {
        registry
            .write_input(
                &r,
                durable_input(
                    subscription,
                    first,
                    client,
                    RequestId::new(),
                    seq,
                    if seq == 0 {
                        operation
                    } else {
                        OperationId::new()
                    },
                ),
                b"a",
                0,
                &mut writer,
            )
            .unwrap();
    }

    registry.disconnect(first, &mut Writer::default());
    let second = ConnectionId::new();
    let fresh = registry.attach(&r, second).unwrap().subscription;
    // The same client's first input on the fresh connection is sequence zero
    // again, and it is a new operation that reaches the PTY.
    assert_eq!(
        registry
            .write_input(
                &r,
                durable_input(
                    fresh,
                    second,
                    client,
                    RequestId::new(),
                    0,
                    OperationId::new()
                ),
                b"b",
                0,
                &mut writer,
            )
            .unwrap(),
        InputAck::Written
    );
    assert_eq!(writer.written, b"aaab");
    // The operation issued on the previous connection is still resolvable.
    assert_eq!(
        registry.input_outcome(&r, client, operation, 0).unwrap(),
        Some(InputAck::Written)
    );
}

#[test]
fn detach_preserves_the_client_ledger_and_disconnect_discards_it() {
    let r = reference();
    let mut registry = registry(r.clone());
    let connection = ConnectionId::new();
    let client = ClientId::new();
    let mut writer = Writer::default();
    let attached = registry
        .attach_for_client(&r, connection, client, None, &mut Writer::default())
        .unwrap();
    assert_eq!(attached.next_input_seq, Some(0));
    registry
        .write_input(
            &r,
            input(
                attached.subscription,
                connection,
                client,
                RequestId::new(),
                0,
            ),
            b"a",
            1,
            &mut writer,
        )
        .unwrap();

    registry
        .detach(
            &r,
            attached.subscription,
            connection,
            &mut Writer::default(),
        )
        .unwrap();
    let reattached = registry
        .attach_for_client(&r, connection, client, None, &mut Writer::default())
        .unwrap();
    assert_eq!(reattached.next_input_seq, Some(1));

    registry.disconnect(connection, &mut Writer::default());
    let fresh = ConnectionId::new();
    let reattached = registry
        .attach_for_client(&r, fresh, client, None, &mut Writer::default())
        .unwrap();
    assert_eq!(reattached.next_input_seq, Some(0));
}

/// Every outcome replays as itself. A cached non-success is never promoted to
/// a success, and an exit after the write does not change the answer either.
#[test]
fn failed_and_ambiguous_finals_replay_unchanged_after_exit() {
    for (failure, expected) in [
        (Some(0), InputAck::Failed),
        (Some(1), InputAck::Ambiguous { applied_prefix: 1 }),
    ] {
        let r = reference();
        let mut registry = registry(r.clone());
        let connection = ConnectionId::new();
        let subscription = registry.attach(&r, connection).unwrap().subscription;
        let client = ClientId::new();
        let operation = OperationId::new();
        let mut writer = Writer {
            failure,
            ..Writer::default()
        };
        assert_eq!(
            registry
                .write_input(
                    &r,
                    durable_input(
                        subscription,
                        connection,
                        client,
                        RequestId::new(),
                        0,
                        operation
                    ),
                    b"ab",
                    0,
                    &mut writer,
                )
                .unwrap(),
            expected
        );
        registry.exited(&r, 0).unwrap();
        // The write path is closed after exit, but the outcome the client is
        // owed was recorded before it and stays reachable.
        assert_eq!(
            registry.input_outcome(&r, client, operation, 10).unwrap(),
            Some(expected.clone())
        );
    }
}

/// Identity reuse for different content, another terminal, or another client
/// is fail-closed: nothing is written and nothing is replayed.
#[test]
fn operation_identity_reuse_conflicts_and_never_crosses_scope() {
    let first_ref = reference();
    let mut registry = registry(first_ref.clone());
    let second_ref = reference();
    registry
        .register(second_ref.clone(), Geometry { cols: 80, rows: 24 })
        .unwrap();
    let connection = ConnectionId::new();
    let first = registry
        .attach(&first_ref, connection)
        .unwrap()
        .subscription;
    let second = registry
        .attach(&second_ref, connection)
        .unwrap()
        .subscription;
    let client = ClientId::new();
    let other_client = ClientId::new();
    let operation = OperationId::new();
    let mut writer = Writer::default();
    registry
        .write_input(
            &first_ref,
            durable_input(first, connection, client, RequestId::new(), 0, operation),
            b"one",
            0,
            &mut writer,
        )
        .unwrap();

    // Same identity, different bytes.
    assert_eq!(
        registry.write_input(
            &first_ref,
            durable_input(first, connection, client, RequestId::new(), 1, operation),
            b"two",
            0,
            &mut writer,
        ),
        Err(RegistryError::IdempotencyConflict)
    );
    // Same identity, another terminal: the ledger is registry-wide, so this
    // is a conflict rather than a fresh write applied to the wrong PTY.
    assert_eq!(
        registry.write_input(
            &second_ref,
            durable_input(second, connection, client, RequestId::new(), 0, operation),
            b"one",
            0,
            &mut writer,
        ),
        Err(RegistryError::IdempotencyConflict)
    );
    // Another client's identical operation is a different operation.
    assert_eq!(
        registry
            .write_input(
                &first_ref,
                durable_input(
                    first,
                    connection,
                    other_client,
                    RequestId::new(),
                    0,
                    operation
                ),
                b"one",
                0,
                &mut writer,
            )
            .unwrap(),
        InputAck::Written
    );
    // Queries never cross terminal or client scope either.
    assert_eq!(
        registry.input_outcome(&second_ref, client, operation, 0),
        Ok(None)
    );
    assert_eq!(writer.written, b"oneone");
}

/// The ledger is bounded on every dimension, and an operation it released is
/// answered as unknown rather than as a success or another operation's final.
#[test]
fn the_operation_ledger_is_bounded_by_count_bytes_and_age() {
    let r = reference();
    let mut registry = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 16)
        .with_input_operation_bounds(InputOperationBounds {
            max_operations: 2,
            max_operations_per_client: 2,
            max_bytes: 8,
            max_age_ms: 1_000,
        });
    registry
        .register(r.clone(), Geometry { cols: 80, rows: 24 })
        .unwrap();
    let connection = ConnectionId::new();
    let subscription = registry.attach(&r, connection).unwrap().subscription;
    let client = ClientId::new();
    let mut writer = Writer::default();
    let operations: Vec<OperationId> = (0..3).map(|_| OperationId::new()).collect();
    for (seq, operation) in operations.iter().enumerate() {
        registry
            .write_input(
                &r,
                durable_input(
                    subscription,
                    connection,
                    client,
                    RequestId::new(),
                    seq as u64,
                    *operation,
                ),
                b"ab",
                0,
                &mut writer,
            )
            .unwrap();
    }
    // The count bound released the oldest; the newest two are still answered.
    assert_eq!(
        registry.input_outcome(&r, client, operations[0], 0),
        Ok(None)
    );
    assert_eq!(
        registry
            .input_outcome(&r, client, operations[2], 0)
            .unwrap(),
        Some(InputAck::Written)
    );
    // The age bound releases the rest, and an aged-out record is unknown.
    assert_eq!(
        registry.input_outcome(&r, client, operations[2], 5_000),
        Ok(None)
    );

    // The byte bound alone also evicts, without the count bound firing.
    let mut wide = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 16)
        .with_input_operation_bounds(InputOperationBounds {
            max_operations: 64,
            max_operations_per_client: 64,
            max_bytes: 4,
            max_age_ms: 1_000,
        });
    wide.register(r.clone(), Geometry { cols: 80, rows: 24 })
        .unwrap();
    let connection = ConnectionId::new();
    let subscription = wide.attach(&r, connection).unwrap().subscription;
    let big = [OperationId::new(), OperationId::new()];
    for (seq, operation) in big.iter().enumerate() {
        wide.write_input(
            &r,
            durable_input(
                subscription,
                connection,
                client,
                RequestId::new(),
                seq as u64,
                *operation,
            ),
            b"abcd",
            0,
            &mut writer,
        )
        .unwrap();
    }
    assert_eq!(wide.input_outcome(&r, client, big[0], 0), Ok(None));
    assert_eq!(
        wide.input_outcome(&r, client, big[1], 0).unwrap(),
        Some(InputAck::Written)
    );
}

/// The remaining ledger dimensions: a per-client cap, and a ledger with no
/// capacity at all, which records nothing rather than growing.
#[test]
fn the_operation_ledger_bounds_each_client_and_can_be_disabled() {
    let r = reference();
    let client = ClientId::new();
    let mut writer = Writer::default();
    // A per-client bound tighter than the aggregate evicts that client only.
    let mut per_client = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 16)
        .with_input_operation_bounds(InputOperationBounds {
            max_operations: 64,
            max_operations_per_client: 1,
            max_bytes: 4_096,
            max_age_ms: 1_000,
        });
    per_client
        .register(r.clone(), Geometry { cols: 80, rows: 24 })
        .unwrap();
    let connection = ConnectionId::new();
    let subscription = per_client.attach(&r, connection).unwrap().subscription;
    let mine = [OperationId::new(), OperationId::new()];
    for (seq, operation) in mine.iter().enumerate() {
        per_client
            .write_input(
                &r,
                durable_input(
                    subscription,
                    connection,
                    client,
                    RequestId::new(),
                    seq as u64,
                    *operation,
                ),
                b"a",
                0,
                &mut writer,
            )
            .unwrap();
    }
    let others = OperationId::new();
    per_client
        .write_input(
            &r,
            durable_input(
                subscription,
                connection,
                other(),
                RequestId::new(),
                // A different client incarnation starts its own sequence.
                0,
                others,
            ),
            b"a",
            0,
            &mut writer,
        )
        .unwrap();
    assert_eq!(per_client.input_outcome(&r, client, mine[0], 0), Ok(None));
    assert_eq!(
        per_client.input_outcome(&r, client, mine[1], 0).unwrap(),
        Some(InputAck::Written)
    );

    // A zero-capacity ledger records nothing at all instead of growing.
    let mut disabled = TerminalRegistry::new(MAX_RETAINED_OUTPUT_BYTES, 16)
        .with_input_operation_bounds(InputOperationBounds {
            max_operations: 0,
            max_operations_per_client: 0,
            max_bytes: 0,
            max_age_ms: 0,
        });
    disabled
        .register(r.clone(), Geometry { cols: 80, rows: 24 })
        .unwrap();
    let connection = ConnectionId::new();
    let subscription = disabled.attach(&r, connection).unwrap().subscription;
    let dropped = OperationId::new();
    disabled
        .write_input(
            &r,
            durable_input(
                subscription,
                connection,
                client,
                RequestId::new(),
                0,
                dropped,
            ),
            b"a",
            0,
            &mut writer,
        )
        .unwrap();
    assert_eq!(disabled.input_outcome(&r, client, dropped, 0), Ok(None));
}

/// A query still fences the terminal identity, and a legacy input without an
/// operation identity records nothing to replay.
#[test]
fn queries_fence_the_terminal_and_legacy_input_records_nothing() {
    let r = reference();
    let mut registry = registry(r.clone());
    let mut stale = r.clone();
    stale.worktree_id = WorktreeId::new();
    let client = ClientId::new();
    assert_eq!(
        registry.input_outcome(&stale, client, OperationId::new(), 0),
        Err(RegistryError::StaleTarget)
    );
    let connection = ConnectionId::new();
    let subscription = registry.attach(&r, connection).unwrap().subscription;
    registry
        .write_input(
            &r,
            input(subscription, connection, client, RequestId::new(), 0),
            b"a",
            0,
            &mut Writer::default(),
        )
        .unwrap();
    assert_eq!(
        registry.input_outcome(&r, client, OperationId::new(), 0),
        Ok(None)
    );
}

fn other() -> ClientId {
    ClientId::new()
}

#[test]
fn stale_refs_and_wrong_attachment_are_rejected() {
    let r = reference();
    let mut registry = registry(r.clone());
    let mut stale = r.clone();
    stale.worktree_id = WorktreeId::new();
    assert_eq!(registry.snapshot(&stale), Err(RegistryError::StaleTarget));
    assert_eq!(
        registry.write_input(
            &r,
            input(1, ConnectionId::new(), ClientId::new(), RequestId::new(), 0),
            b"x",
            0,
            &mut Writer::default()
        ),
        Err(RegistryError::NotAttached)
    );
}
/// A registry whose journal is large enough that a resync can only come
/// from the shared viewport moving, never from trimmed output.
fn shared_registry(reference: TerminalRef) -> TerminalRegistry {
    let mut registry = TerminalRegistry::new(1024, 2);
    registry
        .register(reference, Geometry { cols: 80, rows: 24 })
        .unwrap();
    registry
}

/// One window: its connection, its client incarnation, and its pane size.
fn window(cols: u16, rows: u16) -> (ConnectionId, ClientId, Geometry) {
    (
        ConnectionId::new(),
        ClientId::new(),
        Geometry { cols, rows },
    )
}

#[test]
fn two_windows_share_the_smallest_viewport_and_the_larger_one_is_resynced() {
    let r = reference();
    let mut registry = shared_registry(r.clone());
    let mut pty = Writer::default();
    let (wide_connection, wide_client, wide) = window(80, 24);
    let (narrow_connection, narrow_client, narrow) = window(40, 10);

    // The first window opens the terminal at its own size.
    registry
        .attach_for_client(
            &r,
            wide_connection,
            wide_client,
            None,
            &mut Writer::default(),
        )
        .unwrap();
    let first = registry
        .resize(&r, wide, Some(&wide_client), &mut pty)
        .unwrap();
    assert_eq!(first.geometry, wide);
    // The PTY was already registered at this size, so nothing was applied.
    assert_eq!(pty.resized, Vec::new());
    assert_eq!(registry.replay_from(&r, 0, Some(&wide_client)), Ok(vec![]));

    // A second window opens the same terminal in a smaller pane. The PTY
    // takes the minimum, so neither window is handed more than it can draw,
    // and the attach that stated the claim already answers at that size.
    let second = registry
        .attach_for_client(&r, narrow_connection, narrow_client, Some(narrow), &mut pty)
        .unwrap()
        .snapshot;
    assert_eq!(pty.resized, vec![narrow]);
    assert_eq!(second.geometry, narrow);
    assert_eq!(second.revision, first.revision + 1);

    // The wide window is still decoding at 80x24, so its incremental poll
    // fails closed instead of feeding it output produced for a 40 column
    // grid — the corruption this fixes.
    registry.append_output(&r, b"shared".to_vec()).unwrap();
    assert_eq!(
        registry.replay_from(&r, 0, Some(&wide_client)),
        Err(RegistryError::ResyncRequired)
    );
    // The narrow window asked for this geometry and already has it.
    assert_eq!(
        registry.replay_from(&r, 0, Some(&narrow_client)).unwrap()[0].data,
        b"shared"
    );

    // Reattaching hands the wide window the shared screen and lets it stream
    // again, at the geometry the PTY actually holds.
    let reattached = registry
        .attach_for_client(
            &r,
            wide_connection,
            wide_client,
            None,
            &mut Writer::default(),
        )
        .unwrap();
    assert_eq!(reattached.snapshot.geometry, narrow);
    assert!(registry.replay_from(&r, 0, Some(&wide_client)).is_ok());

    // A repeated request that the shared minimum overrules changes nothing:
    // no PTY call, no revision, and therefore no resync for either window.
    let repeated = registry
        .resize(&r, wide, Some(&wide_client), &mut pty)
        .unwrap();
    assert_eq!(pty.resized, vec![narrow]);
    assert_eq!(repeated.revision, second.revision);
    assert!(registry.replay_from(&r, 0, Some(&narrow_client)).is_ok());

    // A third window asking for exactly the size the terminal already holds
    // changes nothing, and changes nothing again when it leaves: an
    // unchanged minimum commits no PTY call and no revision, so no peer is
    // sent to resync for it.
    let (third_connection, third_client) = (ConnectionId::new(), ClientId::new());
    registry
        .attach_for_client(
            &r,
            third_connection,
            third_client,
            None,
            &mut Writer::default(),
        )
        .unwrap();
    registry
        .resize(&r, narrow, Some(&third_client), &mut pty)
        .unwrap();
    registry.disconnect(third_connection, &mut pty);
    assert_eq!(pty.resized, vec![narrow]);
    assert_eq!(registry.snapshot(&r).unwrap().revision, second.revision);
    assert!(registry.replay_from(&r, 0, Some(&narrow_client)).is_ok());
}

#[test]
fn a_window_that_closes_gives_the_terminal_back_to_the_one_that_stays() {
    let r = reference();
    let mut registry = shared_registry(r.clone());
    let mut pty = Writer::default();
    let (wide_connection, wide_client, wide) = window(100, 30);
    let (narrow_connection, narrow_client, narrow) = window(40, 10);
    for (connection, client, geometry) in [
        (wide_connection, wide_client, wide),
        (narrow_connection, narrow_client, narrow),
    ] {
        registry
            .attach_for_client(&r, connection, client, None, &mut Writer::default())
            .unwrap();
        registry
            .resize(&r, geometry, Some(&client), &mut pty)
            .unwrap();
    }
    let subscription = registry
        .attach_for_client(
            &r,
            narrow_connection,
            narrow_client,
            None,
            &mut Writer::default(),
        )
        .unwrap()
        .subscription;
    assert_eq!(pty.resized, vec![wide, narrow]);
    // The large window has taken the shared screen after being resynced onto
    // it, which is where a second window normally sits.
    registry
        .attach_for_client(
            &r,
            wide_connection,
            wide_client,
            None,
            &mut Writer::default(),
        )
        .unwrap();
    assert!(registry.replay_from(&r, 0, Some(&wide_client)).is_ok());

    // Detaching the small window releases the constraint it held.
    registry
        .detach(&r, subscription, narrow_connection, &mut pty)
        .unwrap();
    assert_eq!(pty.resized, vec![wide, narrow, wide]);
    assert_eq!(registry.snapshot(&r).unwrap().geometry, wide);
    // The window that stayed is told to take a fresh screen, because the
    // geometry moved under it.
    assert_eq!(
        registry.replay_from(&r, 0, Some(&wide_client)),
        Err(RegistryError::ResyncRequired)
    );
    registry
        .attach_for_client(
            &r,
            wide_connection,
            wide_client,
            None,
            &mut Writer::default(),
        )
        .unwrap();
    assert!(registry.replay_from(&r, 0, Some(&wide_client)).is_ok());

    // A window that is gone entirely (its transport dropped) releases the
    // same way, and a terminal nobody constrains keeps its geometry.
    registry
        .resize(&r, narrow, Some(&narrow_client), &mut pty)
        .unwrap();
    assert_eq!(pty.resized, vec![wide, narrow, wide, narrow]);
    registry.disconnect(wide_connection, &mut pty);
    assert_eq!(pty.resized, vec![wide, narrow, wide, narrow]);
    registry.disconnect(narrow_connection, &mut pty);
    assert_eq!(registry.snapshot(&r).unwrap().geometry, narrow);
}

#[test]
fn a_question_does_not_make_a_stale_window_current() {
    let r = reference();
    let mut registry = shared_registry(r.clone());
    let mut pty = Writer::default();
    let (wide_connection, wide_client, wide) = window(100, 30);
    let (narrow_connection, narrow_client, narrow) = window(40, 10);
    for (connection, client, geometry) in [
        (wide_connection, wide_client, wide),
        (narrow_connection, narrow_client, narrow),
    ] {
        registry
            .attach_for_client(&r, connection, client, None, &mut Writer::default())
            .unwrap();
        registry
            .resize(&r, geometry, Some(&client), &mut pty)
            .unwrap();
    }
    assert_eq!(
        registry.replay_from(&r, 0, Some(&wide_client)),
        Err(RegistryError::ResyncRequired)
    );

    // The large window resizes before it has taken a fresh screen. Its
    // request still loses to the smaller one, so nothing moves — and a
    // request that moves nothing must not clear the resync marker, or the
    // window would resume onto a screen it reflowed at the wrong moment.
    registry
        .resize(
            &r,
            Geometry { cols: 90, rows: 20 },
            Some(&wide_client),
            &mut pty,
        )
        .unwrap();
    assert_eq!(pty.resized, vec![wide, narrow]);
    assert_eq!(
        registry.replay_from(&r, 0, Some(&wide_client)),
        Err(RegistryError::ResyncRequired)
    );

    // Only taking the screen clears it.
    registry
        .attach_for_client(
            &r,
            wide_connection,
            wide_client,
            None,
            &mut Writer::default(),
        )
        .unwrap();
    assert!(registry.replay_from(&r, 0, Some(&wide_client)).is_ok());
}

#[test]
fn a_reattaching_window_takes_the_terminal_back_after_the_peer_leaves() {
    // The regression this covers: a window that backgrounds a pane loses its
    // viewport claim, so the reattach has to re-state it. Otherwise a peer's
    // smaller viewport outlives the peer and the pane stays shrunken.
    let r = reference();
    let mut registry = shared_registry(r.clone());
    let mut pty = Writer::default();
    let (wide_connection, wide_client, wide) = window(100, 30);
    let (narrow_connection, narrow_client, narrow) = window(40, 10);
    let subscription = registry
        .attach_for_client(
            &r,
            wide_connection,
            wide_client,
            None,
            &mut Writer::default(),
        )
        .unwrap()
        .subscription;
    registry
        .resize(&r, wide, Some(&wide_client), &mut pty)
        .unwrap();

    // The large window backgrounds this terminal, and the small one takes it.
    registry
        .detach(&r, subscription, wide_connection, &mut pty)
        .unwrap();
    registry
        .attach_for_client(
            &r,
            narrow_connection,
            narrow_client,
            None,
            &mut Writer::default(),
        )
        .unwrap();
    registry
        .resize(&r, narrow, Some(&narrow_client), &mut pty)
        .unwrap();
    assert_eq!(pty.resized, vec![wide, narrow]);

    // Foregrounding it again states the large viewport on the attach itself,
    // so closing the small window gives the terminal back rather than
    // leaving it shrunken forever.
    registry
        .attach_for_client(&r, wide_connection, wide_client, Some(wide), &mut pty)
        .unwrap();
    registry.disconnect(narrow_connection, &mut pty);
    assert_eq!(pty.resized, vec![wide, narrow, wide]);
    assert_eq!(registry.snapshot(&r).unwrap().geometry, wide);
}

#[test]
fn a_pty_that_refuses_the_shared_size_keeps_the_geometry_clients_decode_at() {
    let r = reference();
    let mut registry = shared_registry(r.clone());
    let mut pty = Writer::default();
    let (connection, client, pane) = window(40, 10);
    registry
        .attach_for_client(&r, connection, client, None, &mut Writer::default())
        .unwrap();
    pty.resize_failure = true;

    assert_eq!(
        registry.resize(&r, pane, Some(&client), &mut pty),
        Err(RegistryError::PtyResizeFailed)
    );
    let unchanged = registry.snapshot(&r).unwrap();
    assert_eq!(unchanged.geometry, Geometry { cols: 80, rows: 24 });
    // Nothing moved, so the client is not sent chasing a screen it already
    // has; it retries the request on its own backoff.
    assert!(registry.replay_from(&r, 0, Some(&client)).is_ok());

    // The same refusal during a reconcile leaves the committed geometry
    // alone rather than committing a size the PTY never took.
    let (other_connection, other_client, other) = window(20, 5);
    pty.resize_failure = false;
    registry
        .attach_for_client(
            &r,
            other_connection,
            other_client,
            None,
            &mut Writer::default(),
        )
        .unwrap();
    let subscription = registry
        .resize(&r, other, Some(&other_client), &mut pty)
        .map(|_| 2)
        .unwrap();
    pty.resize_failure = true;
    registry
        .detach(&r, subscription, other_connection, &mut pty)
        .unwrap();
    assert_eq!(registry.snapshot(&r).unwrap().geometry, other);
}

#[test]
fn a_window_that_never_states_a_viewport_never_shrinks_the_terminal() {
    let r = reference();
    let mut registry = shared_registry(r.clone());
    let mut pty = Writer::default();
    let (watcher_connection, watcher_client, _) = window(1, 1);
    let (connection, client, pane) = window(120, 40);

    // A client that only attaches states no requirement of its own.
    registry
        .attach_for_client(
            &r,
            watcher_connection,
            watcher_client,
            None,
            &mut Writer::default(),
        )
        .unwrap();
    registry
        .attach_for_client(&r, connection, client, None, &mut Writer::default())
        .unwrap();
    registry.resize(&r, pane, Some(&client), &mut pty).unwrap();
    assert_eq!(pty.resized, vec![pane]);

    // A viewport stated by a client that never attached is dropped by the
    // next reconcile, so it cannot hold the terminal down forever.
    let stranger = ClientId::new();
    registry
        .resize(&r, Geometry { cols: 8, rows: 2 }, Some(&stranger), &mut pty)
        .unwrap();
    assert_eq!(pty.resized, vec![pane, Geometry { cols: 8, rows: 2 }]);
    registry.disconnect(watcher_connection, &mut pty);
    assert_eq!(pty.resized, vec![pane, Geometry { cols: 8, rows: 2 }, pane]);
}

#[test]
fn an_exited_terminal_is_not_reshaped_by_a_window_that_leaves() {
    let r = reference();
    let mut registry = shared_registry(r.clone());
    let mut pty = Writer::default();
    let (connection, client, pane) = window(40, 10);
    let subscription = registry
        .attach_for_client(&r, connection, client, None, &mut Writer::default())
        .unwrap()
        .subscription;
    let (peer_connection, peer_client, peer) = window(100, 30);
    registry
        .attach_for_client(
            &r,
            peer_connection,
            peer_client,
            None,
            &mut Writer::default(),
        )
        .unwrap();
    registry
        .resize(&r, peer, Some(&peer_client), &mut pty)
        .unwrap();
    registry.resize(&r, pane, Some(&client), &mut pty).unwrap();
    registry.exited(&r, 0).unwrap();

    registry
        .detach(&r, subscription, connection, &mut pty)
        .unwrap();

    // The PTY is gone: its final screen stays exactly as the child left it.
    assert_eq!(pty.resized, vec![peer, pane]);
    assert_eq!(registry.snapshot(&r).unwrap().geometry, pane);
}

#[test]
fn resize_and_exit_follow_final_output() {
    let r = reference();
    let mut registry = registry(r.clone());
    registry.append_output(&r, b"done".to_vec()).unwrap();
    let mut writer = Writer::default();
    let snapshot = registry
        .resize(
            &r,
            Geometry {
                cols: 100,
                rows: 30,
            },
            None,
            &mut writer,
        )
        .unwrap();
    assert_eq!(snapshot.geometry.cols, 100);
    assert_eq!(
        registry.exited(&r, 0).unwrap(),
        Event::Exited {
            terminal: r.clone(),
            revision: 2,
            final_output_offset: 4,
            status: 0,
        }
    );
    let connection = ConnectionId::new();
    let subscription = registry.attach(&r, connection).unwrap().subscription;
    assert_eq!(
        registry.write_input(
            &r,
            input(
                subscription,
                connection,
                ClientId::new(),
                RequestId::new(),
                0
            ),
            b"x",
            0,
            &mut Writer::default()
        ),
        Err(RegistryError::Exited)
    );
    assert_eq!(
        registry.resize(
            &r,
            Geometry { cols: 1, rows: 1 },
            None,
            &mut Writer::default()
        ),
        Err(RegistryError::Exited)
    );
}
