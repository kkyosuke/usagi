//! Deterministic, horizontally scrollable world used by the spacious Garden layout.
//!
//! The compact Garden keeps its fixed plots for the 64x13 minimum surface.  Once the
//! terminal has enough room, this module treats each session plot as a home burrow and
//! lets Agent rabbits travel through a shared landscape.  The simulation is derived
//! entirely from the stable runtime identity and the injected tick: refreshes do not
//! move rabbits unpredictably, reduced motion can freeze the world, and tests can
//! replay every activity without owning a wall clock.

use unicode_width::UnicodeWidthChar;
use usagi_core::domain::agent::AgentStatus as DispatchAgentStatus;
use usagi_core::domain::id::{AgentRuntimeId, SessionId};
use usagi_core::domain::session_lifecycle::{AgentPhase, SessionLifecycle};

use crate::presentation::theme::{Role, Style, garden_rabbit_style};

use super::agent_status;
use super::button::InlineButton;
use super::garden::{
    ANIMATION_CYCLE_TICKS, GardenFrame, GardenHitbox, GardenScrollHitbox, GardenSession,
};
use super::{clip_to_width, display_width, pad_to_width};

const WORLD_MIN_WIDTH: usize = 80;
const WORLD_MIN_HEIGHT: usize = 18;
const SIDE_PADDING: usize = 2;
const HEADER_ROWS: usize = 1;
const FOOTER_ROWS: usize = 2;
const PAN_STEP: usize = 16;
const REGION_WIDTH: usize = 96;
const REGION_CONTENT_WIDTH: usize = 92;
const WORLD_MARGIN: usize = 4;
const HOME_WIDTH: usize = 28;
const HOME_HEIGHT: usize = 4;
const RABBIT_HEIGHT: usize = 4;
const MAX_WORLD_AGENTS_PER_SESSION: usize = 6;
const LIFESTYLE_CYCLE_TICKS: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Point {
    x: i64,
    y: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorldLayout {
    viewport_width: usize,
    world_width: usize,
    world_height: usize,
    scroll: usize,
    max_scroll: usize,
    camera_x: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Places {
    burrow: Point,
    home: Point,
    water: Point,
    food: Point,
    shade: Point,
    pond: Point,
    bed: Point,
    tree: Point,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Facing {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Activity {
    Walking,
    Drinking,
    Eating,
    Sleeping,
    Waiting,
    Interrupted,
    Working,
    Celebrating,
}

impl Activity {
    const fn label(self) -> &'static str {
        match self {
            Self::Walking => "walking",
            Self::Drinking => "drinking",
            Self::Eating => "eating",
            Self::Sleeping => "sleeping",
            Self::Waiting => "waiting",
            Self::Interrupted => "interrupted",
            Self::Working => "running",
            Self::Celebrating => "PR merged!",
        }
    }

    fn label_style(self) -> Style {
        match self {
            Self::Walking | Self::Working => Role::Success.style().bold(),
            Self::Drinking => Role::Info.style(),
            Self::Eating | Self::Celebrating => Role::Success.style(),
            Self::Waiting | Self::Interrupted => Role::Warning.style(),
            Self::Sleeping => Style::new().dim(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Motion {
    point: Point,
    facing: Facing,
    activity: Activity,
}

#[derive(Debug, Clone)]
struct Rabbit {
    session_id: SessionId,
    runtime_id: AgentRuntimeId,
    motion: Motion,
    style: Style,
    tick: u64,
}

struct WorldContents {
    home_hitboxes: Vec<GardenHitbox>,
    rabbit_hitboxes: Vec<GardenHitbox>,
    visible_sessions: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cell {
    Empty,
    Glyph {
        scalar: char,
        width: u8,
        style: Style,
    },
    Continuation,
}

struct Canvas {
    width: usize,
    height: usize,
    camera_x: usize,
    cells: Vec<Cell>,
}

impl Canvas {
    fn new(width: usize, height: usize, camera_x: usize) -> Self {
        Self {
            width,
            height,
            camera_x,
            cells: vec![Cell::Empty; width.saturating_mul(height)],
        }
    }

    fn view_column(&self, world_x: i64) -> Option<usize> {
        let camera = i64::try_from(self.camera_x).expect("Garden camera fits i64");
        let column = world_x - camera;
        (column >= 0)
            .then(|| usize::try_from(column).expect("non-negative Garden column fits usize"))
            .filter(|column| *column < self.width)
    }

    fn put(&mut self, world_x: i64, world_y: i64, scalar: char, style: Style) {
        let Some(column) = self.view_column(world_x) else {
            return;
        };
        let Ok(row) = usize::try_from(world_y) else {
            return;
        };
        if row >= self.height {
            return;
        }
        let glyph_width = UnicodeWidthChar::width(scalar).unwrap_or(0);
        if glyph_width == 0 || glyph_width > self.width.saturating_sub(column) {
            return;
        }
        self.clear_cell(row, column);
        if glyph_width == 2 {
            self.clear_cell(row, column + 1);
        }
        let index = row * self.width + column;
        self.cells[index] = Cell::Glyph {
            scalar,
            width: u8::try_from(glyph_width).expect("terminal glyph width fits u8"),
            style,
        };
        if glyph_width == 2 {
            self.cells[index + 1] = Cell::Continuation;
        }
    }

    fn put_if_empty(&mut self, world_x: i64, world_y: i64, scalar: char, style: Style) {
        let Some(column) = self.view_column(world_x) else {
            return;
        };
        let Ok(row) = usize::try_from(world_y) else {
            return;
        };
        if row >= self.height || !matches!(self.cells[row * self.width + column], Cell::Empty) {
            return;
        }
        self.put(world_x, world_y, scalar, style);
    }

    fn clear_cell(&mut self, row: usize, column: usize) {
        let index = row * self.width + column;
        match self.cells[index] {
            Cell::Glyph { width: 2, .. } => {
                self.cells[index] = Cell::Empty;
                if column + 1 < self.width {
                    self.cells[index + 1] = Cell::Empty;
                }
            }
            Cell::Continuation => {
                self.cells[index] = Cell::Empty;
                if column > 0 {
                    self.cells[index - 1] = Cell::Empty;
                }
            }
            Cell::Empty | Cell::Glyph { .. } => self.cells[index] = Cell::Empty,
        }
    }

    fn text(&mut self, world_x: i64, world_y: i64, value: &str, style: Style) {
        let mut x = world_x;
        for scalar in value.chars() {
            let glyph_width = UnicodeWidthChar::width(scalar).unwrap_or(0);
            if glyph_width == 0 {
                continue;
            }
            self.put(x, world_y, scalar, style);
            x += i64::try_from(glyph_width).expect("glyph width fits i64");
        }
    }

    fn lines<const N: usize>(&mut self, origin: Point, lines: [&str; N], style: Style) {
        for (row, line) in lines.into_iter().enumerate() {
            self.text(
                origin.x,
                origin.y + i64::try_from(row).expect("sprite row fits i64"),
                line,
                style,
            );
        }
    }

    fn rows(self) -> Vec<String> {
        (0..self.height)
            .map(|row| {
                let mut line = String::new();
                let mut segment = String::new();
                let mut segment_style = Style::new();
                for column in 0..self.width {
                    let (scalar, style) = match self.cells[row * self.width + column] {
                        Cell::Empty => (' ', Style::new()),
                        Cell::Glyph { scalar, style, .. } => (scalar, style),
                        Cell::Continuation => continue,
                    };
                    if style != segment_style && !segment.is_empty() {
                        line.push_str(&segment_style.paint(&segment));
                        segment.clear();
                    }
                    segment_style = style;
                    segment.push(scalar);
                }
                if !segment.is_empty() {
                    line.push_str(&segment_style.paint(&segment));
                }
                pad_to_width(
                    &format!("{}{}", " ".repeat(SIDE_PADDING), line),
                    self.width + SIDE_PADDING * 2,
                )
            })
            .collect()
    }
}

pub(super) const fn fits(height: usize, width: usize) -> bool {
    height >= WORLD_MIN_HEIGHT && width >= WORLD_MIN_WIDTH
}

fn world_layout(
    height: usize,
    width: usize,
    session_count: usize,
    requested_scroll: usize,
) -> Option<WorldLayout> {
    if !fits(height, width) {
        return None;
    }
    let viewport_width = width.saturating_sub(SIDE_PADDING * 2);
    let world_height = height.saturating_sub(HEADER_ROWS + FOOTER_ROWS);
    let content_width = if session_count == 0 {
        viewport_width
    } else {
        WORLD_MARGIN
            .saturating_add(session_count.saturating_sub(1).saturating_mul(REGION_WIDTH))
            .saturating_add(REGION_CONTENT_WIDTH)
    };
    let world_width = viewport_width.max(content_width);
    let max_camera = world_width.saturating_sub(viewport_width);
    let max_scroll = max_camera.div_ceil(PAN_STEP);
    let scroll = requested_scroll.min(max_scroll);
    let camera_x = scroll.saturating_mul(PAN_STEP).min(max_camera);
    Some(WorldLayout {
        viewport_width,
        world_width,
        world_height,
        scroll,
        max_scroll,
        camera_x,
    })
}

pub(super) fn render(
    height: usize,
    width: usize,
    workspace_name: &str,
    sessions: &[GardenSession],
    requested_scroll: usize,
    tick: u64,
    reduced_motion: bool,
) -> Option<GardenFrame> {
    let layout = world_layout(height, width, sessions.len(), requested_scroll)?;
    let mut canvas = Canvas::new(layout.viewport_width, layout.world_height, layout.camera_x);
    draw_meadow(&mut canvas, layout, workspace_name);

    let mut contents = draw_sessions(&mut canvas, layout, sessions, tick, reduced_motion);
    contents.rabbit_hitboxes.extend(contents.home_hitboxes);

    let mut rows = Vec::with_capacity(height);
    rows.push(header_line(width, workspace_name, sessions));
    rows.extend(canvas.rows());
    if sessions.is_empty() {
        let message_row = HEADER_ROWS + layout.world_height / 2;
        if let Some(row) = rows.get_mut(message_row) {
            *row = centered(
                width,
                &Style::new().dim().paint("No sessions in the garden"),
            );
        }
    }
    let footer_row = height - FOOTER_ROWS;
    let (scroll_footer, scroll_hitboxes) = scroll_footer(width, footer_row, layout);
    rows.push(scroll_footer);
    rows.push(footer_line(width, layout.max_scroll > 0));

    Some(GardenFrame {
        rows,
        hitboxes: contents.rabbit_hitboxes,
        scroll_hitboxes,
        scroll: layout.scroll,
        max_scroll: layout.max_scroll,
        hidden_sessions: sessions.len().saturating_sub(contents.visible_sessions),
    })
}

fn draw_sessions(
    canvas: &mut Canvas,
    layout: WorldLayout,
    sessions: &[GardenSession],
    tick: u64,
    reduced_motion: bool,
) -> WorldContents {
    let mut home_hitboxes = Vec::new();
    let mut rabbits = Vec::new();
    for (index, session) in sessions.iter().enumerate() {
        let places = places(index, layout.world_height);
        draw_paths(canvas, places);
        draw_pond(canvas, places.pond);
        draw_food_bed(canvas, places.bed);
        draw_tree(canvas, places.tree);
        draw_home(canvas, session, places.burrow);

        if let Some((column, row, hitbox_width, hitbox_height)) =
            clipped_rect(places.burrow, HOME_WIDTH, HOME_HEIGHT, layout)
        {
            home_hitboxes.push(GardenHitbox {
                session_id: session.id,
                agent: None,
                column,
                row,
                width: hitbox_width,
                height: hitbox_height,
            });
        }

        if !session.agents_observed || session.lifecycle != SessionLifecycle::Available {
            draw_lifecycle_pose(canvas, session, places.burrow, tick, reduced_motion);
            continue;
        }

        let agents = agent_status::ordered(&session.agents);
        let visible_count = agents.len().min(MAX_WORLD_AGENTS_PER_SESSION);
        for (agent_index, agent) in agents.into_iter().take(visible_count).enumerate() {
            let seed = stable_hash(&agent.runtime_id.as_str());
            let targets = offset_places(places, agent_index, visible_count);
            let motion = agent_motion(
                agent.phase,
                session.agent_status,
                session.pr_merged,
                targets,
                tick,
                seed,
                reduced_motion,
            );
            rabbits.push(Rabbit {
                session_id: session.id,
                runtime_id: agent.runtime_id,
                motion,
                style: garden_rabbit_style(seed).bold(),
                tick: if reduced_motion { 0 } else { tick },
            });
        }
    }

    let rabbit_hitboxes = draw_rabbits(canvas, layout, &mut rabbits);
    // A rabbit can walk into the viewport while its burrow is just outside it.
    // Count the session as visible whenever either target was actually drawn.
    let visible_sessions = sessions
        .iter()
        .filter(|session| {
            home_hitboxes
                .iter()
                .chain(&rabbit_hitboxes)
                .any(|hitbox| hitbox.session_id == session.id)
        })
        .count();
    WorldContents {
        home_hitboxes,
        rabbit_hitboxes,
        visible_sessions,
    }
}

fn draw_rabbits(
    canvas: &mut Canvas,
    layout: WorldLayout,
    rabbits: &mut [Rabbit],
) -> Vec<GardenHitbox> {
    rabbits.sort_by(|left, right| {
        left.motion
            .point
            .y
            .cmp(&right.motion.point.y)
            .then_with(|| left.motion.point.x.cmp(&right.motion.point.x))
            .then_with(|| left.runtime_id.as_str().cmp(&right.runtime_id.as_str()))
    });
    let mut rabbit_hitboxes = Vec::new();
    for rabbit in rabbits {
        let sprite = rabbit_sprite(rabbit.motion, rabbit.tick);
        let sprite_width = sprite
            .iter()
            .map(|row| display_width(row))
            .max()
            .unwrap_or(0);
        let label_y = rabbit.motion.point.y - 1;
        let label = rabbit.motion.activity.label();
        let label_x = rabbit.motion.point.x
            + i64::try_from(sprite_width.saturating_sub(display_width(label)) / 2)
                .expect("activity label offset fits i64");
        canvas.text(
            label_x,
            label_y,
            label,
            rabbit.motion.activity.label_style(),
        );
        canvas.lines(rabbit.motion.point, sprite, rabbit.style);
        if let Some((column, row, hitbox_width, hitbox_height)) =
            clipped_rect(rabbit.motion.point, sprite_width, RABBIT_HEIGHT, layout)
        {
            rabbit_hitboxes.push(GardenHitbox {
                session_id: rabbit.session_id,
                agent: Some(rabbit.runtime_id),
                column,
                row,
                width: hitbox_width,
                height: hitbox_height,
            });
        }
    }
    // Later rabbits are painted over earlier ones, so their click rectangles win too.
    rabbit_hitboxes.reverse();
    rabbit_hitboxes
}

pub(super) fn canonical_tick(
    height: usize,
    width: usize,
    sessions: &[GardenSession],
    requested_scroll: usize,
    tick: u64,
    reduced_motion: bool,
) -> Option<u64> {
    let tick = tick % ANIMATION_CYCLE_TICKS;
    let expected = render(
        height,
        width,
        "canonical",
        sessions,
        requested_scroll,
        tick,
        reduced_motion,
    )?;
    if reduced_motion {
        return Some(0);
    }
    let visible_animation = sessions.iter().any(|session| {
        session_may_animate(session)
            && expected
                .hitboxes
                .iter()
                .any(|hitbox| hitbox.session_id == session.id)
    });
    if !visible_animation {
        return Some(0);
    }
    let mut canonical = tick;
    // The longest held world activity is twenty ticks. Looking back a little
    // further keeps canonicalization bounded while still folding every held pose.
    for distance in 1..=24 {
        let candidate = (tick + ANIMATION_CYCLE_TICKS - distance) % ANIMATION_CYCLE_TICKS;
        let same = render(
            height,
            width,
            "canonical",
            sessions,
            requested_scroll,
            candidate,
            reduced_motion,
        )? == expected;
        if !same {
            break;
        }
        canonical = candidate;
    }
    Some(canonical)
}

fn session_may_animate(session: &GardenSession) -> bool {
    if !session.agents_observed {
        return false;
    }
    if session.pr_merged {
        return true;
    }
    if matches!(
        session.agent_status,
        Some(DispatchAgentStatus::Idle | DispatchAgentStatus::Exited | DispatchAgentStatus::Failed)
    ) {
        return false;
    }
    match session.lifecycle {
        SessionLifecycle::Creating
        | SessionLifecycle::Initializing
        | SessionLifecycle::Deleting => true,
        SessionLifecycle::Failed => false,
        SessionLifecycle::Available => session.agents.iter().any(|agent| {
            matches!(
                agent.phase,
                AgentPhase::Running | AgentPhase::Waiting | AgentPhase::Absent | AgentPhase::Ready
            )
        }),
    }
}

fn places(index: usize, world_height: usize) -> Places {
    let region = i64::try_from(WORLD_MARGIN + index.saturating_mul(REGION_WIDTH))
        .expect("Garden region fits i64");
    let height = i64::try_from(world_height).expect("Garden height fits i64");
    let home_y = if world_height >= 20 && index % 2 == 1 {
        height - i64::try_from(HOME_HEIGHT).expect("home height fits i64")
    } else {
        1
    };
    let pond_y = (height - 3).max(1);
    let food_y = if index.is_multiple_of(2) {
        2
    } else {
        (height / 2 - 1).max(2)
    };
    let tree_y = (height / 2 - 3).max(1);
    Places {
        burrow: Point {
            x: region,
            y: home_y,
        },
        home: Point {
            x: region + 18,
            y: if home_y == 1 {
                (home_y + 5).min((height - 4).max(1))
            } else {
                (home_y - 5).max(1)
            },
        },
        water: Point {
            x: region + 17,
            y: (pond_y - 3).max(1),
        },
        food: Point {
            x: region + 39,
            y: food_y,
        },
        shade: Point {
            x: region + 60,
            y: (tree_y + 3).min((height - 4).max(1)),
        },
        pond: Point {
            x: region + 24,
            y: pond_y,
        },
        bed: Point {
            x: region + 48,
            y: food_y,
        },
        tree: Point {
            x: region + 63,
            y: tree_y,
        },
    }
}

fn offset_places(mut places: Places, agent_index: usize, agent_count: usize) -> Places {
    let index = i64::try_from(agent_index).expect("rabbit slot fits i64");
    let count = i64::try_from(agent_count).expect("rabbit count fits i64");
    let horizontal = index * 8 - count.saturating_sub(1) * 4;
    for point in [
        &mut places.home,
        &mut places.water,
        &mut places.food,
        &mut places.shade,
    ] {
        point.x += horizontal;
    }
    places
}

fn agent_motion(
    phase: AgentPhase,
    dispatch_status: Option<DispatchAgentStatus>,
    pr_merged: bool,
    places: Places,
    tick: u64,
    seed: u64,
    reduced_motion: bool,
) -> Motion {
    if pr_merged {
        return Motion {
            point: places.home,
            facing: Facing::Right,
            activity: Activity::Celebrating,
        };
    }
    match dispatch_status {
        Some(DispatchAgentStatus::Idle | DispatchAgentStatus::Exited) => {
            return Motion {
                point: places.shade,
                facing: Facing::Right,
                activity: Activity::Sleeping,
            };
        }
        Some(DispatchAgentStatus::Failed) => {
            return Motion {
                point: places.home,
                facing: Facing::Right,
                activity: Activity::Interrupted,
            };
        }
        Some(DispatchAgentStatus::Starting | DispatchAgentStatus::Running) | None => {}
    }
    if reduced_motion {
        let (point, activity) = match phase {
            AgentPhase::Waiting => (places.home, Activity::Waiting),
            AgentPhase::Interrupted => (places.home, Activity::Interrupted),
            AgentPhase::Ended | AgentPhase::Exited | AgentPhase::Sleeping => {
                (places.shade, Activity::Sleeping)
            }
            AgentPhase::Absent | AgentPhase::Ready => (places.home, Activity::Sleeping),
            AgentPhase::Running => (places.home, Activity::Working),
        };
        return Motion {
            point,
            facing: Facing::Right,
            activity,
        };
    }
    match phase {
        AgentPhase::Waiting => Motion {
            point: places.home,
            facing: Facing::Right,
            activity: Activity::Waiting,
        },
        AgentPhase::Interrupted => Motion {
            point: places.home,
            facing: Facing::Right,
            activity: Activity::Interrupted,
        },
        AgentPhase::Sleeping | AgentPhase::Ended | AgentPhase::Exited => Motion {
            point: places.shade,
            facing: Facing::Right,
            activity: Activity::Sleeping,
        },
        AgentPhase::Absent | AgentPhase::Ready | AgentPhase::Running => {
            let local_tick = (tick + seed % LIFESTYLE_CYCLE_TICKS) % LIFESTYLE_CYCLE_TICKS;
            lifestyle_motion(places, local_tick)
        }
    }
}

fn lifestyle_motion(places: Places, tick: u64) -> Motion {
    match tick {
        0..=14 => walking(places.home, places.water, tick, 15),
        15..=24 => Motion {
            point: places.water,
            facing: Facing::Right,
            activity: Activity::Drinking,
        },
        25..=44 => walking(places.water, places.food, tick - 25, 20),
        45..=54 => Motion {
            point: places.food,
            facing: Facing::Right,
            activity: Activity::Eating,
        },
        55..=69 => walking(places.food, places.shade, tick - 55, 15),
        70..=79 => Motion {
            point: places.shade,
            facing: Facing::Right,
            activity: Activity::Sleeping,
        },
        80..=99 => walking(places.shade, places.home, tick - 80, 20),
        _ => unreachable!("lifestyle tick is reduced modulo its cycle"),
    }
}

fn walking(from: Point, to: Point, elapsed: u64, duration: u64) -> Motion {
    Motion {
        point: Point {
            x: lerp(from.x, to.x, elapsed, duration),
            y: lerp(from.y, to.y, elapsed, duration),
        },
        facing: if to.x >= from.x {
            Facing::Right
        } else {
            Facing::Left
        },
        activity: Activity::Walking,
    }
}

fn lerp(from: i64, to: i64, elapsed: u64, duration: u64) -> i64 {
    let delta = i128::from(to - from);
    let elapsed = i128::from(elapsed.min(duration));
    let duration = i128::from(duration.max(1));
    from + i64::try_from(delta * elapsed / duration).expect("Garden interpolation fits i64")
}

fn rabbit_sprite(motion: Motion, tick: u64) -> [&'static str; RABBIT_HEIGHT] {
    match motion.activity {
        Activity::Walking => match (motion.facing, tick.is_multiple_of(2)) {
            (Facing::Right, true) => ["", " /)/)  >", "( o.o)/", " /  \\"],
            (Facing::Right, false) => [" /)/) __", "( o.o)/", "  /  >", ""],
            (Facing::Left, true) => ["", "<  (\\(\\", "\\(.o )", " /  \\"],
            (Facing::Left, false) => ["__(\\(\\", " \\(.o )", " <  \\ ", ""],
        },
        Activity::Drinking => ["", " /)/)", "( . .)__", " /   \\~~"],
        Activity::Eating => [" Y", " /)/)", "( o.o)<Y", "c(\")(\")"],
        Activity::Sleeping => [" z", " /)/)", "( -.-)", "c(\")(\")"],
        Activity::Waiting if tick % 6 == 5 => [" ?", " /)(/", "( o.o)?", "c(\")(\")"],
        Activity::Waiting => [" ?", " /)/)", "( o.o)?", "c(\")(\")"],
        Activity::Interrupted => [" !", " /)/)", "( -.-)!", "c(\")(\")"],
        Activity::Working => ["", " /)/)", "( o.o)", " / > <"],
        Activity::Celebrating if tick.is_multiple_of(2) => {
            [" *  . *", "  /)/)", " \\(^o^)/", " c(\")(\")"]
        }
        Activity::Celebrating => ["  \\ /", "  /)/)", " \\(^.^)/", " c(\")(\")"],
    }
}

fn draw_meadow(canvas: &mut Canvas, layout: WorldLayout, workspace_name: &str) {
    let seed = stable_hash(workspace_name);
    let start = layout.camera_x;
    let end = (layout.camera_x + layout.viewport_width).min(layout.world_width);
    for world_x in start..end {
        for world_y in 0..layout.world_height {
            let mixed = seed
                ^ u64::try_from(world_x)
                    .unwrap_or_default()
                    .wrapping_mul(0x9e37_79b9)
                ^ u64::try_from(world_y).unwrap_or_default().rotate_left(17);
            if mixed.is_multiple_of(97) {
                canvas.put_if_empty(
                    i64::try_from(world_x).expect("Garden x fits i64"),
                    i64::try_from(world_y).expect("Garden y fits i64"),
                    if mixed.is_multiple_of(5) { '*' } else { '.' },
                    Style::new().dim(),
                );
            } else if mixed.is_multiple_of(53) {
                canvas.put_if_empty(
                    i64::try_from(world_x).expect("Garden x fits i64"),
                    i64::try_from(world_y).expect("Garden y fits i64"),
                    'v',
                    Role::Success.style().dim(),
                );
            }
        }
    }
}

fn draw_paths(canvas: &mut Canvas, places: Places) {
    draw_path(canvas, places.burrow, places.home);
    draw_path(canvas, places.home, places.water);
    draw_path(canvas, places.water, places.food);
    draw_path(canvas, places.food, places.shade);
    draw_path(canvas, places.shade, places.home);
}

fn draw_path(canvas: &mut Canvas, from: Point, to: Point) {
    let dx = (to.x - from.x).abs();
    let sx = if from.x < to.x { 1 } else { -1 };
    let dy = -(to.y - from.y).abs();
    let sy = if from.y < to.y { 1 } else { -1 };
    let mut error = dx + dy;
    let (mut x, mut y, mut step) = (from.x, from.y, 0usize);
    loop {
        if step.is_multiple_of(2) {
            canvas.put_if_empty(x, y + 3, '.', Role::Warning.style().dim());
        }
        if x == to.x && y == to.y {
            break;
        }
        let doubled = error * 2;
        if doubled >= dy {
            error += dy;
            x += sx;
        }
        if doubled <= dx {
            error += dx;
            y += sy;
        }
        step += 1;
    }
}

fn draw_pond(canvas: &mut Canvas, origin: Point) {
    canvas.lines(
        origin,
        ["  ~~~~~~~~~~~~~~", " ~  ~~~~~~~~  ~", "  ~~~~~~~~~~~~"],
        Role::Info.style(),
    );
}

fn draw_food_bed(canvas: &mut Canvas, origin: Point) {
    canvas.lines(
        origin,
        ["+--------------+", "| Y  v  Y  v   |", "+--------------+"],
        Role::Success.style().dim(),
    );
}

fn draw_tree(canvas: &mut Canvas, origin: Point) {
    canvas.lines(
        origin,
        ["  &&&", " &&&&&", "   ||", "   ||"],
        Role::Success.style().dim(),
    );
}

fn draw_home(canvas: &mut Canvas, session: &GardenSession, origin: Point) {
    let label = clip_to_width(&session.label, HOME_WIDTH.saturating_sub(6));
    canvas.text(
        origin.x,
        origin.y,
        &format!("-- {label} --"),
        Style::new().dim(),
    );
    let (status, style) = home_status(session);
    canvas.text(
        origin.x + 1,
        origin.y + 1,
        &clip_to_width(&status, HOME_WIDTH.saturating_sub(2)),
        style,
    );
    canvas.lines(
        Point {
            x: origin.x + 4,
            y: origin.y + 2,
        },
        ["   ___", " /     \\"],
        Role::Warning.style().dim(),
    );
}

fn home_status(session: &GardenSession) -> (String, Style) {
    if !session.agents_observed {
        let label = match session.lifecycle {
            SessionLifecycle::Available => "project inactive",
            SessionLifecycle::Creating | SessionLifecycle::Initializing => "cached · creating",
            SessionLifecycle::Deleting => "cached · deleting",
            SessionLifecycle::Failed => "cached · failed",
        };
        return (label.to_owned(), Style::new().dim());
    }
    match session.lifecycle {
        SessionLifecycle::Creating | SessionLifecycle::Initializing => {
            return ("growing".to_owned(), Role::Warning.style());
        }
        SessionLifecycle::Deleting => {
            return ("heading home".to_owned(), Style::new().dim());
        }
        SessionLifecycle::Failed => {
            let label = session.failure_summary.as_deref().map_or_else(
                || "failed".to_owned(),
                |summary| format!("failed · {summary}"),
            );
            return (label, Role::Danger.style().bold());
        }
        SessionLifecycle::Available => {}
    }
    if session.pr_merged {
        return ("PR merged!".to_owned(), Role::Success.style().bold());
    }
    if let Some(status) = session.agent_status {
        match status {
            DispatchAgentStatus::Starting => {
                return ("starting".to_owned(), Role::Accent.style().bold());
            }
            DispatchAgentStatus::Idle => return ("idle".to_owned(), Style::new().dim()),
            DispatchAgentStatus::Exited => return ("stopped".to_owned(), Style::new().dim()),
            DispatchAgentStatus::Failed => {
                return ("failed".to_owned(), Role::Danger.style().bold());
            }
            DispatchAgentStatus::Running => {}
        }
    }
    if session.agents.is_empty() {
        return ("no agents".to_owned(), Style::new().dim());
    }
    (agent_status::summary(&session.agents), Style::new().dim())
}

fn draw_lifecycle_pose(
    canvas: &mut Canvas,
    session: &GardenSession,
    home: Point,
    tick: u64,
    reduced_motion: bool,
) {
    if !session.agents_observed {
        return;
    }
    let phase = if reduced_motion {
        0
    } else {
        (tick + stable_hash(&session.id.as_str())) % 6
    };
    let (sprite, style) = match session.lifecycle {
        SessionLifecycle::Creating | SessionLifecycle::Initializing if phase >= 3 => (
            ["", "  /)/)", " _( . .)_", "__/   \\__"],
            Role::Warning.style(),
        ),
        SessionLifecycle::Creating | SessionLifecycle::Initializing => {
            (["", "", "  /)/)", "__(_ _)__"], Role::Warning.style())
        }
        SessionLifecycle::Deleting => {
            let style = if reduced_motion || phase >= 4 {
                Style::new().dim()
            } else if phase >= 2 {
                Role::Feature.style().dim()
            } else {
                Role::Feature.style()
            };
            (["", " /)/)", "( . .)", "c(\")(\")"], style)
        }
        SessionLifecycle::Failed => (["", " /)/)", "( x.x)", "c(\")(\")/"], Role::Danger.style()),
        SessionLifecycle::Available => return,
    };
    let canvas_height = i64::try_from(canvas.height).expect("Garden canvas height fits i64");
    let pose_y = if home.y
        + i64::try_from(HOME_HEIGHT + RABBIT_HEIGHT).expect("Garden pose height fits i64")
        <= canvas_height
    {
        home.y + i64::try_from(HOME_HEIGHT).expect("home height fits i64")
    } else {
        (home.y - i64::try_from(RABBIT_HEIGHT).expect("rabbit height fits i64")).max(0)
    };
    canvas.lines(
        Point {
            x: home.x + 8,
            y: pose_y,
        },
        sprite,
        style,
    );
}

fn clipped_rect(
    origin: Point,
    width: usize,
    height: usize,
    layout: WorldLayout,
) -> Option<(usize, usize, usize, usize)> {
    let camera = i64::try_from(layout.camera_x).expect("Garden camera fits i64");
    let viewport_end =
        camera + i64::try_from(layout.viewport_width).expect("Garden viewport width fits i64");
    let world_height = i64::try_from(layout.world_height).expect("Garden height fits i64");
    let right = origin.x + i64::try_from(width).expect("Garden rectangle width fits i64");
    let bottom = origin.y + i64::try_from(height).expect("Garden rectangle height fits i64");
    let left = origin.x.max(camera);
    let top = origin.y.max(0);
    let right = right.min(viewport_end);
    let bottom = bottom.min(world_height);
    if left >= right || top >= bottom {
        return None;
    }
    Some((
        SIDE_PADDING + usize::try_from(left - camera).expect("visible Garden column fits usize"),
        HEADER_ROWS + usize::try_from(top).expect("visible Garden row fits usize"),
        usize::try_from(right - left).expect("visible Garden width fits usize"),
        usize::try_from(bottom - top).expect("visible Garden height fits usize"),
    ))
}

fn header_line(width: usize, workspace_name: &str, sessions: &[GardenSession]) -> String {
    let running = sessions
        .iter()
        .filter(|session| {
            session.lifecycle == SessionLifecycle::Available
                && !matches!(
                    session.agent_status,
                    Some(
                        DispatchAgentStatus::Starting
                            | DispatchAgentStatus::Idle
                            | DispatchAgentStatus::Exited
                            | DispatchAgentStatus::Failed
                    )
                )
        })
        .flat_map(|session| &session.agents)
        .filter(|agent| agent.phase == AgentPhase::Running)
        .count();
    let left = Role::Feature.style().bold().paint(&format!(
        " usagi / {}",
        clip_to_width(workspace_name, width / 2)
    ));
    let right = Style::new()
        .dim()
        .paint(&format!("{} sessions · {running} running ", sessions.len()));
    let gap = width.saturating_sub(display_width(&left) + display_width(&right));
    pad_to_width(&format!("{left}{}{right}", " ".repeat(gap)), width)
}

fn scroll_footer(
    width: usize,
    row: usize,
    layout: WorldLayout,
) -> (String, Vec<GardenScrollHitbox>) {
    if layout.max_scroll == 0 {
        return (" ".repeat(width), Vec::new());
    }
    let previous = InlineButton::new("← Pan");
    let next = InlineButton::new("Pan →");
    let previous_rendered = previous.render(
        width,
        if layout.scroll == 0 {
            Style::new().dim()
        } else {
            Role::Feature.style()
        },
    );
    let next_rendered = next.render(
        width,
        if layout.scroll == layout.max_scroll {
            Style::new().dim()
        } else {
            Role::Feature.style()
        },
    );
    let first = layout.camera_x + 1;
    let last = (layout.camera_x + layout.viewport_width).min(layout.world_width);
    let indicator = Style::new()
        .dim()
        .paint(&format!("· {first}-{last} / {} ·", layout.world_width));
    let content_width = previous_rendered.width + display_width(&indicator) + next_rendered.width;
    let left = width.saturating_sub(content_width) / 2;
    let next_column = left + previous_rendered.width + display_width(&indicator);
    let line = pad_to_width(
        &format!(
            "{}{}{}{}",
            " ".repeat(left),
            previous_rendered.line,
            indicator,
            next_rendered.line
        ),
        width,
    );
    (
        line,
        vec![
            GardenScrollHitbox {
                scroll: layout.scroll.saturating_sub(1),
                column: left,
                row,
                width: previous_rendered.width,
                height: 1,
            },
            GardenScrollHitbox {
                scroll: (layout.scroll + 1).min(layout.max_scroll),
                column: next_column,
                row,
                width: next_rendered.width,
                height: 1,
            },
        ],
    )
}

fn footer_line(width: usize, pannable: bool) -> String {
    let left = Role::Feature.style().paint(if pannable {
        " Garden · click a usagi · ←/→ pan"
    } else {
        " Garden · click a usagi"
    });
    let right = Style::new().dim().paint("any key · wake ");
    let gap = width.saturating_sub(display_width(&left) + display_width(&right));
    pad_to_width(&format!("{left}{}{right}", " ".repeat(gap)), width)
}

fn centered(width: usize, value: &str) -> String {
    let value = clip_to_width(value, width);
    let padding = width.saturating_sub(display_width(&value)) / 2;
    pad_to_width(&format!("{}{value}", " ".repeat(padding)), width)
}

fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Activity, Facing, LIFESTYLE_CYCLE_TICKS, Places, Point, canonical_tick, fits,
        lifestyle_motion, render,
    };
    use crate::presentation::widgets::display_width;
    use crate::presentation::widgets::garden::{GardenAgent, GardenSession};
    use usagi_core::domain::agent::AgentStatus as DispatchAgentStatus;
    use usagi_core::domain::id::{AgentRuntimeId, SessionId};
    use usagi_core::domain::session_lifecycle::{AgentPhase, SessionLifecycle};

    const SESSION_ID: &str = "00000000-0000-4000-8000-000000000001";
    const AGENT_ID: &str = "10000000-0000-4000-8000-000000000001";

    fn places() -> Places {
        Places {
            burrow: Point { x: 1, y: 1 },
            home: Point { x: 4, y: 2 },
            water: Point { x: 24, y: 10 },
            food: Point { x: 44, y: 3 },
            shade: Point { x: 64, y: 8 },
            pond: Point { x: 30, y: 12 },
            bed: Point { x: 52, y: 3 },
            tree: Point { x: 66, y: 5 },
        }
    }

    fn session(id: &str, label: &str) -> GardenSession {
        GardenSession {
            id: SessionId::parse(id).expect("fixture session id"),
            label: label.to_owned(),
            lifecycle: SessionLifecycle::Available,
            selected: false,
            failure_summary: None,
            agents_observed: true,
            agents: vec![GardenAgent {
                runtime_id: AgentRuntimeId::parse(AGENT_ID).expect("fixture agent id"),
                phase: AgentPhase::Running,
            }],
            agent_status: None,
            pr_merged: false,
        }
    }

    fn plain(rows: &[String]) -> String {
        rows.iter()
            .map(|row| {
                let mut out = String::new();
                let mut chars = row.chars();
                while let Some(character) = chars.next() {
                    if character == '\u{1b}' {
                        for candidate in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&candidate) && candidate != '[' {
                                break;
                            }
                        }
                    } else {
                        out.push(character);
                    }
                }
                out
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn spacious_terminals_use_the_world_but_compact_terminals_keep_plots() {
        assert!(fits(24, 100));
        assert!(!fits(14, 64));
        assert!(!fits(17, 100));
        assert!(!fits(24, 79));
    }

    #[test]
    fn lifestyle_walks_both_directions_and_visits_every_resource() {
        let places = places();
        assert_eq!(lifestyle_motion(places, 0).point, places.home);
        assert_eq!(lifestyle_motion(places, 5).facing, Facing::Right);
        assert_eq!(lifestyle_motion(places, 15).activity, Activity::Drinking);
        assert_eq!(lifestyle_motion(places, 45).activity, Activity::Eating);
        assert_eq!(lifestyle_motion(places, 70).activity, Activity::Sleeping);
        assert_eq!(lifestyle_motion(places, 85).facing, Facing::Left);
        assert_eq!(LIFESTYLE_CYCLE_TICKS, 100);
    }

    #[test]
    fn rendered_world_is_deterministic_width_safe_and_clickable_at_the_rabbit() {
        let session = session(SESSION_ID, "日本語-session");
        let sessions = std::slice::from_ref(&session);
        let first = render(24, 100, "atlas", sessions, 0, 13, false).expect("world fits");
        let second = render(24, 100, "atlas", sessions, 0, 13, false).expect("world fits");
        assert_eq!(first, second);
        assert_eq!(first.rows.len(), 24);
        assert!(first.rows.iter().all(|row| display_width(row) == 100));
        assert!(first.hitboxes.iter().any(|hitbox| hitbox.agent.is_some()));
        let text = plain(&first.rows);
        assert!(text.contains("日本語-session"));
        assert!(text.contains('~'));
        assert!(text.contains('Y'));
        assert!(text.contains("&&&"));
    }

    #[test]
    fn a_rabbit_eventually_walks_drinks_eats_and_sleeps() {
        let active = session(SESSION_ID, "one");
        let sessions = std::slice::from_ref(&active);
        let text = (0..LIFESTYLE_CYCLE_TICKS)
            .map(|tick| {
                plain(
                    &render(24, 100, "atlas", sessions, 0, tick, false)
                        .expect("world fits")
                        .rows,
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        for activity in ["walking", "drinking", "eating", "sleeping"] {
            assert!(text.contains(activity), "missing {activity}");
        }
    }

    #[test]
    fn moving_rabbit_hitboxes_follow_the_sprite_and_reduced_motion_freezes_it() {
        let active = session(SESSION_ID, "one");
        let sessions = std::slice::from_ref(&active);
        let moving = (0..LIFESTYLE_CYCLE_TICKS)
            .map(|tick| {
                render(24, 100, "atlas", sessions, 0, tick, false)
                    .expect("world fits")
                    .hitboxes
                    .into_iter()
                    .find(|hitbox| hitbox.agent.is_some())
                    .map(|hitbox| (hitbox.column, hitbox.row))
                    .expect("rabbit remains visible in its first region")
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert!(moving.len() > 8);

        let first = render(24, 100, "atlas", sessions, 0, 1, true).expect("world fits");
        let later = render(24, 100, "atlas", sessions, 0, 73, true).expect("world fits");
        assert_eq!(first, later);

        let mut celebrating = session(SESSION_ID, "celebrating");
        celebrating.pr_merged = true;
        let first =
            render(24, 100, "atlas", &[celebrating.clone()], 0, 1, true).expect("world fits");
        let later = render(24, 100, "atlas", &[celebrating], 0, 2, true).expect("world fits");
        assert_eq!(first, later);
    }

    #[test]
    fn pan_reaches_the_last_session_home() {
        let sessions = (1..=4)
            .map(|index| {
                session(
                    &format!("{index:08x}-0000-4000-8000-000000000001"),
                    &format!("session-{index}"),
                )
            })
            .collect::<Vec<_>>();
        let first = render(24, 100, "atlas", &sessions, 0, 0, false).expect("world fits");
        assert!(first.max_scroll > 0);
        assert!(first.hidden_sessions > 0);
        let last = render(24, 100, "atlas", &sessions, usize::MAX, 0, false).expect("world fits");
        assert_eq!(last.scroll, last.max_scroll);
        assert!(plain(&last.rows).contains("session-4"));
        assert_eq!(last.scroll_hitboxes.len(), 2);
    }

    #[test]
    fn a_wandering_rabbit_keeps_its_session_visible_after_the_home_leaves_view() {
        let first = session(SESSION_ID, "first");
        let second = session("00000000-0000-4000-8000-000000000002", "second");
        let seed = super::stable_hash(&first.agents[0].runtime_id.as_str());
        let tick =
            (70 + LIFESTYLE_CYCLE_TICKS - seed % LIFESTYLE_CYCLE_TICKS) % LIFESTYLE_CYCLE_TICKS;
        let frame =
            render(24, 80, "atlas", &[first.clone(), second], 2, tick, false).expect("world fits");

        assert!(
            !frame
                .hitboxes
                .iter()
                .any(|hitbox| hitbox.session_id == first.id && hitbox.agent.is_none())
        );
        assert!(
            frame
                .hitboxes
                .iter()
                .any(|hitbox| hitbox.session_id == first.id && hitbox.agent.is_some())
        );
        assert_eq!(frame.hidden_sessions, 0);
    }

    #[test]
    fn all_six_rabbits_stay_inside_the_single_session_world() {
        let mut session = session(SESSION_ID, "six");
        session.agents = (1..=6)
            .map(|index| GardenAgent {
                runtime_id: AgentRuntimeId::parse(&format!(
                    "{index:08x}-0000-4000-8000-000000000001"
                ))
                .expect("fixture agent id"),
                phase: AgentPhase::Running,
            })
            .collect();
        for tick in 0..LIFESTYLE_CYCLE_TICKS {
            let frame = render(
                24,
                100,
                "atlas",
                std::slice::from_ref(&session),
                0,
                tick,
                false,
            )
            .expect("world fits");
            assert_eq!(
                frame
                    .hitboxes
                    .iter()
                    .filter(|hitbox| hitbox.agent.is_some())
                    .count(),
                6,
                "tick {tick}"
            );
        }
    }

    #[test]
    fn terminal_dispatch_states_do_not_keep_the_world_clock_alive() {
        let mut idle = session(SESSION_ID, "idle");
        idle.agent_status = Some(DispatchAgentStatus::Idle);
        assert_eq!(
            canonical_tick(24, 100, std::slice::from_ref(&idle), 0, 73, false),
            Some(0)
        );

        let mut foreground = session(SESSION_ID, "foreground");
        foreground.lifecycle = SessionLifecycle::Failed;
        let background = session("00000000-0000-4000-8000-000000000002", "background");
        assert_eq!(
            canonical_tick(24, 80, &[foreground, background], 0, 73, false),
            Some(0),
            "an off-screen moving rabbit must not keep the visible frame clock alive"
        );
    }

    #[test]
    fn canvas_clips_invalid_cells_and_repairs_wide_glyph_overwrites() {
        let style = super::Style::new();
        let mut canvas = super::Canvas::new(4, 2, 2);
        canvas.put(1, 0, 'x', style);
        canvas.put(2, -1, 'x', style);
        canvas.put(2, 2, 'x', style);
        canvas.put(5, 0, '界', style);
        canvas.put(2, 0, '\u{301}', style);
        canvas.text(2, 0, "\u{301}a", style);
        canvas.put_if_empty(2, -1, 'x', style);
        canvas.put_if_empty(2, 0, 'x', style);

        canvas.put(2, 1, '界', style);
        canvas.put(2, 1, 'b', style);
        canvas.put(2, 1, '界', style);
        canvas.put(3, 1, 'c', style);

        let rows = canvas.rows();
        assert!(rows.iter().all(|row| display_width(row) == 8));
        assert!(plain(&rows).contains('a'));
        assert!(plain(&rows).contains('c'));
    }

    #[test]
    fn home_status_names_every_cached_lifecycle_and_dispatch_override() {
        for (lifecycle, expected) in [
            (SessionLifecycle::Available, "project inactive"),
            (SessionLifecycle::Creating, "cached · creating"),
            (SessionLifecycle::Initializing, "cached · creating"),
            (SessionLifecycle::Deleting, "cached · deleting"),
            (SessionLifecycle::Failed, "cached · failed"),
        ] {
            let mut cached = session(SESSION_ID, "cached");
            cached.lifecycle = lifecycle;
            cached.agents_observed = false;
            assert_eq!(super::home_status(&cached).0, expected);
        }

        for (lifecycle, expected) in [
            (SessionLifecycle::Creating, "growing"),
            (SessionLifecycle::Initializing, "growing"),
            (SessionLifecycle::Deleting, "heading home"),
        ] {
            let mut transitional = session(SESSION_ID, "transition");
            transitional.lifecycle = lifecycle;
            assert_eq!(super::home_status(&transitional).0, expected);
        }

        let mut failed = session(SESSION_ID, "failed");
        failed.lifecycle = SessionLifecycle::Failed;
        assert_eq!(super::home_status(&failed).0, "failed");
        failed.failure_summary = Some("safe summary".to_owned());
        assert_eq!(super::home_status(&failed).0, "failed · safe summary");

        let mut available = session(SESSION_ID, "available");
        available.pr_merged = true;
        assert_eq!(super::home_status(&available).0, "PR merged!");
        available.pr_merged = false;
        for (status, expected) in [
            (DispatchAgentStatus::Starting, "starting"),
            (DispatchAgentStatus::Idle, "idle"),
            (DispatchAgentStatus::Exited, "stopped"),
            (DispatchAgentStatus::Failed, "failed"),
            (DispatchAgentStatus::Running, "1 run"),
        ] {
            available.agent_status = Some(status);
            assert_eq!(super::home_status(&available).0, expected);
        }
        available.agent_status = None;
        available.agents.clear();
        assert_eq!(super::home_status(&available).0, "no agents");
        available.agents.push(GardenAgent {
            runtime_id: AgentRuntimeId::parse(AGENT_ID).expect("fixture agent id"),
            phase: AgentPhase::Interrupted,
        });
        assert_eq!(super::home_status(&available).0, "1 int");
        available.agents = [
            ("019b0c57-6c00-7000-8000-000000000001", AgentPhase::Ready),
            ("019b0c57-6c00-7000-8000-000000000002", AgentPhase::Sleeping),
            ("019b0c57-6c00-7000-8000-000000000003", AgentPhase::Absent),
        ]
        .into_iter()
        .map(|(runtime_id, phase)| GardenAgent {
            runtime_id: AgentRuntimeId::parse(runtime_id).expect("fixture agent id"),
            phase,
        })
        .collect();
        assert_eq!(
            super::home_status(&available).0,
            "1 ready · 1 sleep · 1 idle"
        );
    }

    #[test]
    fn motion_overrides_and_static_world_states_are_explicit() {
        let places = places();
        let motion = |phase, status, merged, reduced| {
            super::agent_motion(phase, status, merged, places, 0, 0, reduced)
        };
        assert_eq!(
            motion(AgentPhase::Running, None, true, false).activity,
            Activity::Celebrating
        );
        for (phase, expected) in [
            (AgentPhase::Waiting, Activity::Waiting),
            (AgentPhase::Interrupted, Activity::Interrupted),
            (AgentPhase::Sleeping, Activity::Sleeping),
            (AgentPhase::Ended, Activity::Sleeping),
            (AgentPhase::Running, Activity::Working),
        ] {
            assert_eq!(motion(phase, None, false, true).activity, expected);
        }
        assert_eq!(
            motion(
                AgentPhase::Running,
                Some(DispatchAgentStatus::Idle),
                false,
                false
            )
            .activity,
            Activity::Sleeping
        );
        assert_eq!(
            motion(
                AgentPhase::Running,
                Some(DispatchAgentStatus::Failed),
                false,
                false
            )
            .activity,
            Activity::Interrupted
        );
        assert_eq!(
            motion(AgentPhase::Interrupted, None, false, false).activity,
            Activity::Interrupted
        );
        assert_eq!(
            motion(AgentPhase::Sleeping, None, false, false).activity,
            Activity::Sleeping
        );
        assert_eq!(
            motion(AgentPhase::Sleeping, None, false, true).point,
            places.shade
        );
        assert_eq!(
            motion(AgentPhase::Ready, None, false, true).point,
            places.home
        );
        assert_eq!(
            motion(
                AgentPhase::Running,
                Some(DispatchAgentStatus::Idle),
                false,
                true
            )
            .point,
            places.shade
        );
        assert_eq!(
            motion(
                AgentPhase::Running,
                Some(DispatchAgentStatus::Failed),
                false,
                true
            )
            .activity,
            Activity::Interrupted
        );

        assert_eq!(Activity::Interrupted.label(), "interrupted");
        assert_eq!(Activity::Celebrating.label(), "PR merged!");
        assert_ne!(
            super::rabbit_sprite(motion(AgentPhase::Running, None, true, false), 0),
            super::rabbit_sprite(motion(AgentPhase::Running, None, true, false), 1)
        );
        assert!(
            super::rabbit_sprite(motion(AgentPhase::Interrupted, None, false, false), 0)
                .join("\n")
                .contains('!')
        );
    }

    #[test]
    fn lifecycle_worlds_cover_animation_clock_and_empty_layout_edges() {
        assert!(super::world_layout(17, 100, 1, 0).is_none());
        let empty = render(24, 100, "atlas", &[], 0, 0, false).expect("world fits");
        assert!(plain(&empty.rows).contains("No sessions in the garden"));

        let lifecycles = [
            SessionLifecycle::Creating,
            SessionLifecycle::Initializing,
            SessionLifecycle::Deleting,
            SessionLifecycle::Failed,
        ];
        let sessions = lifecycles
            .into_iter()
            .enumerate()
            .map(|(index, lifecycle)| {
                let mut value = session(
                    &format!("{index:08x}-0000-4000-8000-000000000001"),
                    "lifecycle",
                );
                value.lifecycle = lifecycle;
                value
            })
            .collect::<Vec<_>>();
        for tick in 0..6 {
            let frame = render(24, 500, "atlas", &sessions, 0, tick, false).expect("world fits");
            assert_eq!(frame.rows.len(), 24);
        }
        let reduced = render(24, 500, "atlas", &sessions, 0, 5, true).expect("world fits");
        assert!(plain(&reduced.rows).contains("heading home"));

        let statuses = [
            DispatchAgentStatus::Starting,
            DispatchAgentStatus::Idle,
            DispatchAgentStatus::Exited,
            DispatchAgentStatus::Failed,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, status)| {
            let mut value = session(
                &format!("{index:08x}-0000-4000-8000-000000000011"),
                "status",
            );
            value.agent_status = Some(status);
            value
        })
        .collect::<Vec<_>>();
        let frame = render(24, 500, "atlas", &statuses, 0, 0, false).expect("world fits");
        assert!(plain(&frame.rows).contains("0 running"));

        let mut available_canvas = super::Canvas::new(96, 21, 0);
        super::draw_lifecycle_pose(
            &mut available_canvas,
            &session(SESSION_ID, "available"),
            Point { x: 4, y: 1 },
            0,
            false,
        );
    }

    #[test]
    fn animation_eligibility_covers_session_and_agent_states() {
        let mut value = session(SESSION_ID, "eligibility");
        value.agents_observed = false;
        assert!(!super::session_may_animate(&value));
        value.agents_observed = true;
        value.pr_merged = true;
        assert!(super::session_may_animate(&value));
        value.pr_merged = false;
        value.agent_status = Some(DispatchAgentStatus::Exited);
        assert!(!super::session_may_animate(&value));
        value.agent_status = None;
        for lifecycle in [
            SessionLifecycle::Creating,
            SessionLifecycle::Initializing,
            SessionLifecycle::Deleting,
        ] {
            value.lifecycle = lifecycle;
            assert!(super::session_may_animate(&value));
        }
        value.lifecycle = SessionLifecycle::Failed;
        assert!(!super::session_may_animate(&value));
        value.lifecycle = SessionLifecycle::Available;
        value.agents[0].phase = AgentPhase::Sleeping;
        assert!(!super::session_may_animate(&value));
        value.agents[0].phase = AgentPhase::Interrupted;
        assert!(!super::session_may_animate(&value));
        value.agents[0].phase = AgentPhase::Ready;
        assert!(super::session_may_animate(&value));

        value.pr_merged = true;
        assert!(canonical_tick(24, 100, &[value], 0, 7, false).is_some());
    }

    #[test]
    fn canonical_clock_rejects_small_worlds_freezes_reduced_motion_and_folds_held_poses() {
        let value = session(SESSION_ID, "canonical");
        assert_eq!(
            canonical_tick(17, 100, std::slice::from_ref(&value), 0, 7, false),
            None
        );
        assert_eq!(
            canonical_tick(24, 100, std::slice::from_ref(&value), 0, 7, true),
            Some(0)
        );

        let seed = super::stable_hash(&value.agents[0].runtime_id.as_str());
        let tick =
            (71 + LIFESTYLE_CYCLE_TICKS - seed % LIFESTYLE_CYCLE_TICKS) % LIFESTYLE_CYCLE_TICKS;
        assert_ne!(
            canonical_tick(24, 100, &[value], 0, tick, false),
            Some(tick),
            "the second sleeping frame should reuse the first sleeping frame's tick"
        );
    }

    #[test]
    #[should_panic(expected = "lifestyle tick is reduced modulo its cycle")]
    fn lifestyle_rejects_a_tick_outside_its_cycle() {
        let _ = lifestyle_motion(places(), LIFESTYLE_CYCLE_TICKS);
    }
}
