use super::*;

use console::Term;

/// A reader that replays a scripted sequence of [`Input`]s (keys, scrolls, or
/// clicks), so the loop's click path can be exercised. Both the blocking and the
/// timeout read drain the same queue; once it empties they fall back to `Ctrl-C`
/// so a test can never spin forever.
struct InputScriptedReader {
    inputs: VecDeque<Input>,
}

impl KeyReader for InputScriptedReader {
    fn read_key(&mut self) -> io::Result<Key> {
        match self.inputs.pop_front() {
            Some(Input::Key(key)) => Ok(key),
            _ => Ok(Key::CtrlC),
        }
    }
    fn read_input(&mut self) -> io::Result<Input> {
        Ok(self.inputs.pop_front().unwrap_or(Input::Key(Key::CtrlC)))
    }
    fn read_input_timeout(&mut self, _t: std::time::Duration) -> io::Result<Option<Input>> {
        Ok(Some(
            self.inputs.pop_front().unwrap_or(Input::Key(Key::CtrlC)),
        ))
    }
}

/// The 0-based `(col, row)` of the resting rabbit's feet in a freshly rendered
/// [`sample_state`] frame for `term`, so a click can target exactly where the
/// rabbit is drawn (matching what the loop hit-tests against).
fn rabbit_feet_cell(term: &Term) -> (u16, u16) {
    let state = sample_state();
    let (h, w) = term.size();
    let frame = ui::render_frame(h as usize, w as usize, &state);
    let (row, line) = frame
        .iter()
        .enumerate()
        .find(|(_, l)| console::strip_ansi_codes(l).contains("o(_(\")(\")"))
        .expect("the rabbit's feet are drawn");
    let col = console::strip_ansi_codes(line)
        .find('o')
        .expect("the feet lead with `o`");
    (col as u16, row as u16)
}

#[test]
fn a_click_on_the_rabbit_is_consumed_and_a_miss_is_ignored() {
    // The loop reads a click that lands on the mascot (it reacts, forcing a
    // repaint) and one that misses (a no-op), then quits on Ctrl-C — proving both
    // click outcomes are handled without dispatching a key.
    let term = Term::stdout();
    let (col, row) = rabbit_feet_cell(&term);
    let hit = Input::Click(ClickEvent { col, row });
    let miss = Input::Click(ClickEvent { col: 0, row: 0 });
    let mut reader = InputScriptedReader {
        inputs: VecDeque::from(vec![hit, miss, Input::Key(Key::CtrlC)]),
    };
    let outcome = run_with_tasks(&TaskHandle::new(), &mut reader, |_, _| {}, |_| {}).unwrap();
    assert!(matches!(outcome, Outcome::Quit));
}

#[test]
fn a_scroll_is_read_and_dropped() {
    // A wheel turn never moves the management screens, so it is read and dropped;
    // the following Ctrl-C still quits.
    let scroll = Input::Scroll(crate::presentation::tui::io::screen::ScrollEvent {
        lines: -3,
        col: 1,
        row: 1,
    });
    let mut reader = InputScriptedReader {
        inputs: VecDeque::from(vec![scroll, Input::Key(Key::CtrlC)]),
    };
    let outcome = run_with_tasks(&TaskHandle::new(), &mut reader, |_, _| {}, |_| {}).unwrap();
    assert!(matches!(outcome, Outcome::Quit));
}

#[test]
fn handle_mascot_click_reacts_on_a_hit_and_ignores_a_miss() {
    let term = Term::stdout();
    let (col, row) = rabbit_feet_cell(&term);
    // A click on the rabbit kicks a reaction and asks for a repaint.
    let mut state = sample_state();
    assert!(handle_mascot_click(
        &term,
        &mut state,
        ClickEvent { col, row }
    ));
    assert!(state.mascot_reacting());
    // A click elsewhere reacts to nothing.
    let mut elsewhere = sample_state();
    assert!(!handle_mascot_click(
        &term,
        &mut elsewhere,
        ClickEvent { col: 0, row: 0 }
    ));
    assert!(!elsewhere.mascot_reacting());
}

#[test]
fn a_click_is_inert_while_an_overlay_is_open() {
    // With the command palette open a click is meant for it (or nothing), not the
    // rabbit beneath — so even a click on the rabbit's cell is a no-op.
    let term = Term::stdout();
    let (col, row) = rabbit_feet_cell(&term);
    let mut state = sample_state();
    assert!(mascot_clickable(&state));
    state.open_command_palette();
    assert!(!mascot_clickable(&state));
    assert!(!handle_mascot_click(
        &term,
        &mut state,
        ClickEvent { col, row }
    ));
    assert!(!state.mascot_reacting());
}

#[test]
fn click_hits_mascot_is_false_when_no_mascot_is_shown() {
    let state = sample_state();
    // A full-size screen shows the mascot, and its feet cell hits it.
    let term = Term::stdout();
    let (col, row) = rabbit_feet_cell(&term);
    let (h, w) = term.size();
    assert!(click_hits_mascot(
        h as usize,
        w as usize,
        &state,
        ClickEvent { col, row }
    ));
    // A point off the rabbit misses even though the mascot is shown.
    assert!(!click_hits_mascot(
        h as usize,
        w as usize,
        &state,
        ClickEvent { col: 0, row: 0 }
    ));
    // A screen too short to fit the mascot has nothing to hit.
    assert!(!click_hits_mascot(
        8,
        80,
        &state,
        ClickEvent { col: 1, row: 1 }
    ));
}
