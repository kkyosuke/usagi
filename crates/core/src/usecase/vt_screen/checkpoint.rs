//! Versioned semantic checkpoint of a [`VtScreen`](super::VtScreen).
//!
//! The daemon owns the PTY and is the grid authority; on attach / resync it
//! must hand a fresh client the *complete* screen state, not a raw byte tail cut
//! at an arbitrary boundary. This module defines that serializable state and the
//! bounds that make decoding a hostile payload safe.
//!
//! The types mirror the parser state: both screen buffers (the always-present
//! `primary` and, while a full-screen app owns the alternate buffer, an
//! `alternate`), each buffer's cursor / saved cursor / scroll region, the
//! scrollback, an interned style table (cells reference it by index), and the
//! decoder's in-flight phase / CSI params / partial UTF-8. Serialize and
//! reconstruct live with the parser (`VtScreen::checkpoint` /
//! `VtScreen::from_checkpoint`) so the daemon and TUI never re-implement them.
//!
//! Decoding a checkpoint follows **arithmetic check → budget check → allocate**:
//! every count, index and `rows × cols` is validated with checked arithmetic and
//! against the limits below *before* any grid is allocated. Out-of-range values,
//! multiplication overflow and unknown `schema_version` fail closed with a typed
//! [`CheckpointError`]; decoding never panics, never allocates unbounded, and
//! never leaves a corrupt parser.

use serde::{Deserialize, Serialize};

/// The checkpoint schema this build produces and accepts. A checkpoint carrying
/// any other version is rejected (`fail closed`) rather than misinterpreted.
pub const SCHEMA_VERSION: u16 = 1;

/// Maximum grid rows. Bounds geometry before the cell multiplication.
pub const ROWS_MAX: u32 = 1024;
/// Maximum grid columns. Bounds geometry before the cell multiplication.
pub const COLS_MAX: u32 = 2048;
/// Maximum visible cells (`rows × cols`) in one buffer. Equal to
/// `ROWS_MAX × COLS_MAX`; the product is validated with `checked_mul` so an
/// overflowing geometry is rejected before it is compared to this budget.
pub const CELLS_PER_TERMINAL_MAX: u32 = ROWS_MAX * COLS_MAX;
/// Maximum retained scrollback rows in one buffer. Matches the live parser cap.
pub const SCROLLBACK_MAX: usize = 10_000;
/// Maximum entries in the interned style table.
pub const STYLES_MAX: usize = 4_096;
/// Maximum length of the decoder's in-flight CSI parameter run, in bytes.
pub const PARAMS_MAX: usize = 64;
/// Maximum buffered partial UTF-8 bytes (a 4-byte sequence has at most 3
/// pending continuation bytes).
pub const UTF8_PENDING_MAX: usize = 3;
/// Maximum length of a complete UTF-8 sequence, so `utf8_needed` is bounded.
pub const UTF8_NEEDED_MAX: u8 = 4;
/// Maximum serialized size of a single checkpoint. The default IPC frame is
/// 1 MiB; this leaves envelope headroom so a checkpoint always fits one frame.
pub const CHECKPOINT_BYTES_MAX: usize = 1024 * 1024 - 4 * 1024;

/// Which buffer is currently displayed. `Primary` is the shell/transcript
/// buffer; `Alternate` is the buffer a full-screen application draws into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActiveBuffer {
    /// The primary (shell / transcript) buffer is displayed.
    Primary,
    /// The alternate (full-screen application) buffer is displayed.
    Alternate,
}

/// Decoder parser position, mirroring the private `Phase` used by the parser.
/// Carried so a checkpoint taken mid-sequence resumes on the correct state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecoderPhase {
    /// Printable text and C0 controls are interpreted directly.
    Ground,
    /// The previous byte was `ESC`.
    Escape,
    /// Collecting a `CSI` parameter/intermediate run.
    Csi,
    /// Swallowing an `OSC` string.
    Osc,
    /// Swallowing the byte after a charset-select escape.
    Charset,
}

/// The visible geometry a checkpoint's buffers were captured at. Both buffers
/// share this geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Geometry {
    /// Number of visible rows.
    pub rows: u32,
    /// Number of visible columns.
    pub cols: u32,
}

/// One run-length span of identical cells: `repeat` cells that share `style_id`,
/// `ch` and `continuation`. Blank padding compresses to a single run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellRun {
    /// Index into [`ScreenCheckpoint::styles`] for these cells' SGR state.
    pub style_id: u32,
    /// The character in each cell (`'\0'` for wide-glyph continuation cells).
    pub ch: char,
    /// Whether these cells are the trailing half of a wide glyph.
    pub continuation: bool,
    /// How many consecutive cells this run represents (at least 1).
    pub repeat: u32,
}

/// One grid row as a run-length sequence of [`CellRun`]s. The runs must expand
/// to exactly `cols` cells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowCheckpoint {
    /// Left-to-right run-length spans covering the row.
    pub runs: Vec<CellRun>,
}

/// A complete screen buffer: its visible grid, scrollback, cursor state and
/// scroll region. Style references are indices into the checkpoint's shared
/// style table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferCheckpoint {
    /// The visible grid, top to bottom (`rows` rows once expanded).
    pub grid: Vec<RowCheckpoint>,
    /// Rows pushed off the top of the grid, oldest first.
    pub scrollback: Vec<RowCheckpoint>,
    /// Zero-based `(row, col)` cursor position.
    pub cursor: (u32, u32),
    /// Cursor position saved by `DECSC` / `SCP`, if any.
    pub saved_cursor: Option<(u32, u32)>,
    /// Inclusive `(top, bottom)` `DECSTBM` scroll region.
    pub scroll_region: (u32, u32),
    /// Index into [`ScreenCheckpoint::styles`] for the SGR state applied to
    /// cells printed next.
    pub style_id: u32,
}

/// The decoder's in-flight state, so a checkpoint taken mid-escape or
/// mid-multibyte resumes correctly when the suffix is fed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecoderCheckpoint {
    /// Parser position when the checkpoint was taken.
    pub phase: DecoderPhase,
    /// Collected CSI parameter/intermediate bytes (without the leading `ESC [`).
    pub params: String,
    /// Partially received UTF-8 bytes awaiting continuation bytes.
    pub utf8_pending: Vec<u8>,
    /// Total length of the multibyte sequence being assembled.
    pub utf8_needed: u8,
}

/// A versioned, self-contained semantic snapshot of a [`VtScreen`](super::VtScreen).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenCheckpoint {
    /// Checkpoint schema version. See [`SCHEMA_VERSION`].
    pub schema_version: u16,
    /// Visible geometry both buffers were captured at.
    pub geometry: Geometry,
    /// Which buffer is currently displayed.
    pub active: ActiveBuffer,
    /// The primary buffer. Always present, even while the alternate is active
    /// (it is then the saved background buffer).
    pub primary: BufferCheckpoint,
    /// The alternate buffer, present exactly when `active` is
    /// [`ActiveBuffer::Alternate`].
    pub alternate: Option<BufferCheckpoint>,
    /// Interned SGR style strings. Cells and buffers reference entries by index.
    pub styles: Vec<String>,
    /// The decoder's in-flight state.
    pub decoder: DecoderCheckpoint,
}

/// A checkpoint that failed validation. Every variant fails closed: the caller
/// keeps its current state and requests a resync rather than reconstructing a
/// corrupt or unbounded screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointError {
    /// `schema_version` is not one this build understands.
    UnknownSchemaVersion {
        /// The version carried by the checkpoint.
        found: u16,
        /// The version this build produces and accepts.
        expected: u16,
    },
    /// `rows` is zero or exceeds [`ROWS_MAX`].
    RowsOutOfRange(u32),
    /// `cols` is zero or exceeds [`COLS_MAX`].
    ColsOutOfRange(u32),
    /// `rows × cols` overflows.
    CellCountOverflow,
    /// `rows × cols` exceeds [`CELLS_PER_TERMINAL_MAX`].
    TooManyCells(u32),
    /// A buffer's scrollback exceeds [`SCROLLBACK_MAX`].
    ScrollbackTooLong(usize),
    /// The style table exceeds [`STYLES_MAX`].
    TooManyStyles(usize),
    /// A `style_id` is past the end of the style table.
    StyleIdOutOfRange {
        /// The offending index.
        id: u32,
        /// The style table length.
        styles: usize,
    },
    /// A row's run repeats overflow while being summed.
    RowRepeatOverflow,
    /// A row's runs do not expand to exactly `cols` cells.
    RowLength {
        /// The expected width (`cols`).
        expected: u32,
        /// The width the runs actually expand to.
        actual: u32,
    },
    /// A buffer's grid does not have exactly `rows` rows.
    GridRowCount {
        /// The expected row count (`rows`).
        expected: u32,
        /// The row count the grid actually has.
        actual: usize,
    },
    /// A cursor or saved cursor is outside the grid.
    CursorOutOfRange {
        /// The offending row.
        row: u32,
        /// The offending column.
        col: u32,
    },
    /// A scroll region is inverted or reaches past the last row.
    ScrollRegionInvalid {
        /// The region top.
        top: u32,
        /// The region bottom.
        bottom: u32,
    },
    /// The decoder's CSI parameter run exceeds [`PARAMS_MAX`].
    ParamsTooLong(usize),
    /// The decoder's partial UTF-8 buffer exceeds [`UTF8_PENDING_MAX`].
    Utf8PendingTooLong(usize),
    /// `utf8_needed` exceeds [`UTF8_NEEDED_MAX`].
    Utf8NeededOutOfRange(u8),
    /// `active` and the presence of `alternate` disagree.
    ActiveBufferMismatch,
    /// A serialized checkpoint exceeds [`CHECKPOINT_BYTES_MAX`].
    TooLarge {
        /// The serialized size.
        size: usize,
        /// The byte limit.
        limit: usize,
    },
    /// The serialized bytes are not a valid checkpoint encoding.
    Malformed(String),
}

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSchemaVersion { found, expected } => {
                write!(
                    f,
                    "unknown checkpoint schema version {found} (expected {expected})"
                )
            }
            Self::RowsOutOfRange(rows) => write!(f, "rows {rows} out of range (1..={ROWS_MAX})"),
            Self::ColsOutOfRange(cols) => write!(f, "cols {cols} out of range (1..={COLS_MAX})"),
            Self::CellCountOverflow => write!(f, "rows × cols overflows"),
            Self::TooManyCells(cells) => {
                write!(f, "cell count {cells} exceeds {CELLS_PER_TERMINAL_MAX}")
            }
            Self::ScrollbackTooLong(len) => {
                write!(f, "scrollback length {len} exceeds {SCROLLBACK_MAX}")
            }
            Self::TooManyStyles(len) => write!(f, "style table length {len} exceeds {STYLES_MAX}"),
            Self::StyleIdOutOfRange { id, styles } => {
                write!(f, "style id {id} out of range (table length {styles})")
            }
            Self::RowRepeatOverflow => write!(f, "row run repeats overflow"),
            Self::RowLength { expected, actual } => {
                write!(f, "row expands to {actual} cells (expected {expected})")
            }
            Self::GridRowCount { expected, actual } => {
                write!(f, "grid has {actual} rows (expected {expected})")
            }
            Self::CursorOutOfRange { row, col } => {
                write!(f, "cursor ({row}, {col}) outside the grid")
            }
            Self::ScrollRegionInvalid { top, bottom } => {
                write!(f, "scroll region ({top}, {bottom}) is invalid")
            }
            Self::ParamsTooLong(len) => {
                write!(f, "decoder params length {len} exceeds {PARAMS_MAX}")
            }
            Self::Utf8PendingTooLong(len) => {
                write!(f, "utf8 pending length {len} exceeds {UTF8_PENDING_MAX}")
            }
            Self::Utf8NeededOutOfRange(needed) => {
                write!(f, "utf8 needed {needed} exceeds {UTF8_NEEDED_MAX}")
            }
            Self::ActiveBufferMismatch => {
                write!(
                    f,
                    "active buffer disagrees with the presence of an alternate buffer"
                )
            }
            Self::TooLarge { size, limit } => {
                write!(f, "serialized checkpoint {size} bytes exceeds {limit}")
            }
            Self::Malformed(reason) => write!(f, "malformed checkpoint: {reason}"),
        }
    }
}

impl std::error::Error for CheckpointError {}

/// The visible geometry validated against the per-terminal budget.
///
/// Returned by [`ScreenCheckpoint::validated_geometry`] so callers can allocate
/// a grid of exactly this size knowing the arithmetic did not overflow and the
/// budget was met.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ValidGeometry {
    pub rows: usize,
    pub cols: usize,
}

impl ScreenCheckpoint {
    /// Validates `schema_version`, geometry and the top-level table lengths in
    /// the design's order — **arithmetic check → budget check** — before any
    /// buffer is expanded. Per-buffer and per-row bounds are enforced during
    /// reconstruction (see `super`).
    pub(super) fn validated_geometry(&self) -> Result<ValidGeometry, CheckpointError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(CheckpointError::UnknownSchemaVersion {
                found: self.schema_version,
                expected: SCHEMA_VERSION,
            });
        }
        let (rows, cols) = (self.geometry.rows, self.geometry.cols);
        // Arithmetic check first: reject a geometry whose product overflows
        // before it is measured against any budget.
        let cells = rows
            .checked_mul(cols)
            .ok_or(CheckpointError::CellCountOverflow)?;
        // Budget checks.
        if cells > CELLS_PER_TERMINAL_MAX {
            return Err(CheckpointError::TooManyCells(cells));
        }
        if rows == 0 || rows > ROWS_MAX {
            return Err(CheckpointError::RowsOutOfRange(rows));
        }
        if cols == 0 || cols > COLS_MAX {
            return Err(CheckpointError::ColsOutOfRange(cols));
        }
        if self.styles.len() > STYLES_MAX {
            return Err(CheckpointError::TooManyStyles(self.styles.len()));
        }
        if self.decoder.params.len() > PARAMS_MAX {
            return Err(CheckpointError::ParamsTooLong(self.decoder.params.len()));
        }
        if self.decoder.utf8_pending.len() > UTF8_PENDING_MAX {
            return Err(CheckpointError::Utf8PendingTooLong(
                self.decoder.utf8_pending.len(),
            ));
        }
        if self.decoder.utf8_needed > UTF8_NEEDED_MAX {
            return Err(CheckpointError::Utf8NeededOutOfRange(
                self.decoder.utf8_needed,
            ));
        }
        // `active` and `alternate` must agree before we decide which buffer maps
        // to the live parser state.
        let has_alternate = self.alternate.is_some();
        if (self.active == ActiveBuffer::Alternate) != has_alternate {
            return Err(CheckpointError::ActiveBufferMismatch);
        }
        Ok(ValidGeometry {
            rows: rows as usize,
            cols: cols as usize,
        })
    }

    /// Serializes to bytes, rejecting a checkpoint that would exceed
    /// [`CHECKPOINT_BYTES_MAX`] so a single checkpoint always fits one frame.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::TooLarge`] when the serialized form exceeds
    /// the byte budget.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, CheckpointError> {
        // `serde_json` serialization of a well-formed struct does not fail.
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        if bytes.len() > CHECKPOINT_BYTES_MAX {
            return Err(CheckpointError::TooLarge {
                size: bytes.len(),
                limit: CHECKPOINT_BYTES_MAX,
            });
        }
        Ok(bytes)
    }

    /// Parses bytes into a checkpoint, rejecting oversized input *before*
    /// deserialization so a hostile payload cannot drive an unbounded parse.
    ///
    /// This checks only the byte budget and encoding; call
    /// [`from_checkpoint`](super::VtScreen::from_checkpoint) to validate the
    /// semantic bounds and reconstruct a screen.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::TooLarge`] when the input exceeds the byte
    /// budget, or [`CheckpointError::Malformed`] when it is not a valid
    /// checkpoint encoding.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, CheckpointError> {
        if bytes.len() > CHECKPOINT_BYTES_MAX {
            return Err(CheckpointError::TooLarge {
                size: bytes.len(),
                limit: CHECKPOINT_BYTES_MAX,
            });
        }
        serde_json::from_slice(bytes).map_err(|error| CheckpointError::Malformed(error.to_string()))
    }
}
