//! The original Garden illustrations in one bounded, shared meadow.
//!
//! Each runtime owns a disjoint roaming area, so even the moving sprites remain
//! entirely visible and independently clickable. Density changes sprite size,
//! never the landscape or the runtime membership.

use super::{
    ANIMATION_CYCLE_TICKS, AgentPhase, DispatchAgentStatus, FOOTER_ROWS, GardenFrame, GardenHitbox,
    GardenSession, HEADER_ROWS, Role, SIDE_PADDING, SessionLifecycle, Style, agent_status,
    clip_to_width, dense_agent_appearance, display_width, footer_line, garden_rabbit_style,
    header_line, pad_to_width, stable_hash,
};
use unicode_width::UnicodeWidthChar;

const SCENERY_HEIGHT: usize = 4;
const RABBIT_HEIGHT: usize = 4;
const LIFESTYLE_CYCLE_TICKS: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Point {
    x: i64,
    y: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Places {
    home: Point,
    water: Point,
    food: Point,
    shade: Point,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Motion {
    point: Point,
    facing: Facing,
    activity: Activity,
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
    cells: Vec<Cell>,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![Cell::Empty; width.saturating_mul(height)],
        }
    }

    fn view_column(&self, world_x: i64) -> Option<usize> {
        let column = world_x;
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

    fn clear_area(&mut self, area: Area) {
        for row in area.y..area.y + area.height {
            for column in area.x..area.x + area.width {
                self.clear_cell(row, column);
            }
        }
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
            (Facing::Left, true) => ["", "< (\\(\\", "\\(.o )", " /  \\"],
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Density {
    Full,
    Small,
    Glyph,
}

impl Density {
    const fn size(self) -> (usize, usize) {
        match self {
            Self::Full => (9, 4),
            Self::Small => (7, 2),
            Self::Glyph => (2, 1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Area {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl Area {
    fn origin(self) -> Point {
        Point {
            x: coordinate(self.x),
            y: coordinate(self.y),
        }
    }

    fn hitbox(self, session: &GardenSession) -> GardenHitbox {
        GardenHitbox {
            session_id: session.id,
            agent: None,
            column: self.x + SIDE_PADDING,
            row: self.y + HEADER_ROWS,
            width: self.width,
            height: self.height,
        }
    }
}

/// Choose roomy, balanced roaming areas instead of packing rabbits against one edge.
fn grid(area: Area, count: usize, minimum: (usize, usize)) -> Vec<Area> {
    if count == 0 {
        return Vec::new();
    }
    let (sprite_width, sprite_height) = minimum;
    let columns = (1..=(area.width / sprite_width).min(count))
        .filter(|columns| count.div_ceil(*columns) <= area.height / sprite_height)
        .max_by_key(|columns| {
            let width = area.width / columns;
            let height = area.height / count.div_ceil(*columns);
            (
                (width - sprite_width).min((height - sprite_height) * 3),
                (width - sprite_width + 1) * (height - sprite_height + 1),
            )
        })
        .expect("the selected density fits every item in its area");
    let rows = count.div_ceil(columns);
    let height = area.height / rows;
    (0..count)
        .map(|index| {
            let row = index / columns;
            let row_columns = (count - row * columns).min(columns);
            let width = area.width / columns;
            Area {
                x: area.x + (area.width - row_columns * width) / 2 + index % columns * width,
                y: area.y + (area.height - rows * height) / 2 + row * height,
                width,
                height,
            }
        })
        .collect()
}

fn coordinate(value: usize) -> i64 {
    i64::try_from(value).expect("Garden dimension fits i64")
}

pub(super) const fn fits(height: usize, width: usize) -> bool {
    height >= 18 && width >= 80
}

pub(super) fn render(
    height: usize,
    width: usize,
    workspace_name: &str,
    sessions: &[GardenSession],
    tick: u64,
    reduced_motion: bool,
) -> GardenFrame {
    render_with_session_homes(
        height,
        width,
        workspace_name,
        sessions,
        tick,
        reduced_motion,
        true,
    )
}

pub(super) fn render_without_session_homes(
    height: usize,
    width: usize,
    workspace_name: &str,
    sessions: &[GardenSession],
    tick: u64,
    reduced_motion: bool,
) -> GardenFrame {
    render_with_session_homes(
        height,
        width,
        workspace_name,
        sessions,
        tick,
        reduced_motion,
        false,
    )
}

fn render_with_session_homes(
    height: usize,
    width: usize,
    workspace_name: &str,
    sessions: &[GardenSession],
    tick: u64,
    reduced_motion: bool,
    show_session_homes: bool,
) -> GardenFrame {
    let tick = if reduced_motion {
        0
    } else {
        tick % ANIMATION_CYCLE_TICKS
    };
    let mut canvas = Canvas::new(width - SIDE_PADDING * 2, height - HEADER_ROWS - FOOTER_ROWS);
    let agents = sessions
        .iter()
        .flat_map(|session| {
            let agents = if session.agents_observed {
                agent_status::ordered(&session.agents)
            } else {
                Vec::new()
            };
            agents.into_iter().map(move |agent| (session, agent))
        })
        .collect::<Vec<_>>();
    let agent_rows = agents.len().div_ceil(canvas.width / 2);
    if agent_rows > canvas.height - SCENERY_HEIGHT {
        // At this physical limit even two-cell rabbits plus the scenery cannot
        // fit. Spend the complete body on Agents instead of overlapping targets.
        return super::render_dense(
            height,
            width,
            workspace_name,
            sessions,
            tick,
            reduced_motion,
        )
        .expect("spacious terminals meet the compact minimum");
    }
    let home_budget = show_session_homes
        .then(|| (canvas.height / 3).min(canvas.height - SCENERY_HEIGHT - agent_rows));
    draw_meadow(&mut canvas, workspace_name, tick);
    draw_pond(
        &mut canvas,
        Point {
            x: coordinate((width - 20) / 2),
            y: 1,
        },
    );
    draw_food_bed(&mut canvas, Point { x: 3, y: 1 });
    let tree_x = coordinate(canvas.width - 10);
    draw_tree(&mut canvas, Point { x: tree_x, y: 0 });

    let (home_areas, home_height) = home_budget.map_or_else(
        || (Vec::new(), 0),
        |budget| homes(&canvas, sessions.len(), budget),
    );
    let meadow = Area {
        x: 0,
        y: SCENERY_HEIGHT,
        width: canvas.width,
        height: canvas.height - SCENERY_HEIGHT - home_height,
    };
    let density = [Density::Full, Density::Small, Density::Glyph]
        .into_iter()
        .find(|density| {
            let (w, h) = density.size();
            agents.len() <= (meadow.width / w) * (meadow.height / h)
        })
        .unwrap_or(Density::Glyph);
    let areas = grid(meadow, agents.len(), density.size());
    let mut hitboxes = Vec::with_capacity(agents.len() + home_areas.len());
    for ((session, agent), area) in agents.into_iter().zip(areas) {
        hitboxes.push(draw_agent(
            &mut canvas,
            session,
            agent,
            area,
            density,
            tick,
            reduced_motion,
        ));
    }
    for (session, area) in sessions.iter().zip(home_areas) {
        draw_home(&mut canvas, session, area);
        hitboxes.push(area.hitbox(session));
    }
    let mut rows = vec![
        header_line(width, workspace_name, sessions),
        super::sky_line(width, workspace_name, tick, reduced_motion),
    ];
    rows.extend(canvas.rows());
    if sessions.is_empty() {
        rows[HEADER_ROWS + SCENERY_HEIGHT + meadow.height / 2] = super::centered(
            width,
            &Style::new().dim().paint("No sessions in the garden"),
        );
    }
    rows.push(footer_line(width));
    GardenFrame { rows, hitboxes }
}

fn draw_agent(
    canvas: &mut Canvas,
    session: &GardenSession,
    agent: super::GardenAgent,
    area: Area,
    density: Density,
    tick: u64,
    reduced_motion: bool,
) -> GardenHitbox {
    let seed = stable_hash(&agent.runtime_id.as_str());
    let places = roaming_places(area, density.size());
    let mut motion = agent_motion(
        agent.phase,
        session.agent_status,
        session.pr_merged,
        places,
        tick,
        seed,
        reduced_motion,
    );
    if session.lifecycle != SessionLifecycle::Available
        || session.agent_status == Some(DispatchAgentStatus::Starting)
    {
        motion = Motion {
            point: places.home,
            facing: Facing::Right,
            activity: Activity::Working,
        };
    }
    let (state_style, _, _, face) = dense_agent_appearance(session, agent);
    let overridden = session.lifecycle != SessionLifecycle::Available
        || matches!(
            session.agent_status,
            Some(DispatchAgentStatus::Starting | DispatchAgentStatus::Failed)
        );
    let style = if overridden {
        state_style
    } else {
        garden_rabbit_style(seed).bold()
    };
    canvas.clear_area(Area {
        x: usize::try_from(motion.point.x).expect("rabbit x fits usize"),
        y: usize::try_from(motion.point.y).expect("rabbit y fits usize"),
        width: density.size().0,
        height: density.size().1,
    });
    let sprite = rabbit_sprite(motion, tick);
    let (rabbit_width, rabbit_height) = match density {
        Density::Full => {
            let sprite = if overridden {
                ["", " /)/)", face, "c(\")(\")"]
            } else {
                sprite
            };
            canvas.lines(motion.point, sprite, style);
            (
                sprite
                    .iter()
                    .map(|row| display_width(row))
                    .max()
                    .unwrap_or(0),
                RABBIT_HEIGHT,
            )
        }
        Density::Small => {
            canvas.lines(motion.point, [" /)/)", face], style);
            (display_width(face).max(5), 2)
        }
        Density::Glyph => {
            canvas.text(motion.point.x, motion.point.y, "兎", state_style);
            (2, 1)
        }
    };
    let mut hitbox = Area {
        x: usize::try_from(motion.point.x).expect("rabbit stays in meadow"),
        y: usize::try_from(motion.point.y).expect("rabbit stays in meadow"),
        width: rabbit_width,
        height: rabbit_height,
    }
    .hitbox(session);
    hitbox.agent = Some(agent.runtime_id);
    hitbox
}

fn homes(canvas: &Canvas, count: usize, budget: usize) -> (Vec<Area>, usize) {
    if count == 0 {
        return (Vec::new(), 0);
    }
    let (minimum_width, home_height) = [(14, 4), (8, 2), (2, 1)]
        .into_iter()
        .find(|(w, h)| count.div_ceil(canvas.width / w) * h <= budget)
        .unwrap_or((2, 1));
    let visible = count.min((canvas.width / minimum_width) * (budget / home_height));
    let height = visible.div_ceil(canvas.width / minimum_width) * home_height;
    let area = Area {
        x: 0,
        y: canvas.height - height,
        width: canvas.width,
        height,
    };
    (grid(area, visible, (minimum_width, home_height)), height)
}

fn roaming_places(area: Area, sprite: (usize, usize)) -> Places {
    let left = coordinate(area.x);
    let right = coordinate(area.x + area.width.saturating_sub(sprite.0));
    let top = coordinate(area.y);
    let bottom = coordinate(area.y + area.height.saturating_sub(sprite.1));
    Places {
        home: Point { x: left, y: bottom },
        water: Point {
            x: left + (right - left) / 3,
            y: top,
        },
        food: Point { x: right, y: top },
        shade: Point {
            x: right - (right - left) / 3,
            y: bottom,
        },
    }
}

fn draw_home(canvas: &mut Canvas, session: &GardenSession, area: Area) {
    canvas.clear_area(area);
    let origin = area.origin();
    if area.height == 1 {
        canvas.text(origin.x, origin.y, "⌒", Role::Warning.style().dim());
        canvas.text(
            origin.x + 1,
            origin.y,
            &clip_to_width(&session.label, area.width - 1),
            Style::new().dim(),
        );
        return;
    }
    canvas.text(
        origin.x,
        origin.y,
        &clip_to_width(&format!("-- {} --", session.label), area.width - 1),
        Style::new().dim(),
    );
    let (status, style) = home_status(session);
    canvas.text(
        origin.x,
        origin.y + 1,
        &clip_to_width(&status, area.width - 1),
        style,
    );
    if area.height >= 4 {
        canvas.lines(
            Point {
                x: origin.x + coordinate((area.width - 8) / 2),
                y: origin.y + 2,
            },
            ["   ___", " /     \\"],
            Role::Warning.style().dim(),
        );
    }
}

fn home_status(session: &GardenSession) -> (String, Style) {
    if !session.agents_observed {
        return (
            super::inactive_status(session).to_owned(),
            Style::new().dim(),
        );
    }
    if session.pending_decisions > 0 {
        let noun = if session.pending_decisions == 1 {
            "decision"
        } else {
            "decisions"
        };
        return (
            format!("action · {} {noun}", session.pending_decisions),
            Role::Warning.style().bold(),
        );
    }
    if session.lifecycle == SessionLifecycle::Failed {
        return (
            session.failure_summary.as_ref().map_or_else(
                || "failed".to_owned(),
                |reason| format!("failed · {reason}"),
            ),
            Role::Danger.style(),
        );
    }
    if session.pr_merged {
        return ("PR merged!".to_owned(), Role::Success.style());
    }
    if session.lifecycle == SessionLifecycle::Available
        && matches!(
            session.agent_status,
            None | Some(DispatchAgentStatus::Running)
        )
        && session.agents.len() > 1
    {
        return (
            format!(
                "{}  {}",
                agent_status::ordered(&session.agents)
                    .iter()
                    .map(|agent| agent_status::glyph(agent.phase))
                    .collect::<Vec<_>>()
                    .join(" "),
                agent_status::summary_parts(&session.agents).join(" · ")
            ),
            Style::new().dim(),
        );
    }
    if let Some(agent) = agent_status::ordered(&session.agents).first() {
        let (style, _, status, _) = dense_agent_appearance(session, *agent);
        return (status.to_owned(), style);
    }
    let (style, status) = super::session_summary(session);
    (status, style)
}

fn draw_meadow(canvas: &mut Canvas, workspace_name: &str, tick: u64) {
    let seed = stable_hash(workspace_name);
    let phase = usize::try_from(tick / 4 % 6).expect("ambient phase fits usize");
    for x in 0..canvas.width {
        for y in 0..canvas.height {
            let mixed = seed
                ^ u64::try_from(x)
                    .expect("x fits u64")
                    .wrapping_mul(0x9e37_79b9)
                ^ u64::try_from(y).expect("y fits u64").rotate_left(17);
            if mixed.is_multiple_of(97) {
                canvas.put_if_empty(
                    coordinate(x),
                    coordinate(y),
                    super::TWINKLE[(phase + x + y) % 6],
                    Style::new().dim(),
                );
            } else if mixed.is_multiple_of(53) {
                canvas.put_if_empty(
                    coordinate(x),
                    coordinate(y),
                    ['v', '\\', '|', '/'][(phase + x) % 4],
                    Role::Success.style().dim(),
                );
            }
        }
    }
}

pub(super) fn canonical_tick(
    height: usize,
    width: usize,
    sessions: &[GardenSession],
    tick: u64,
    reduced_motion: bool,
) -> u64 {
    canonical_tick_with_session_homes(height, width, sessions, tick, reduced_motion, true)
}

pub(super) fn canonical_tick_without_session_homes(
    height: usize,
    width: usize,
    sessions: &[GardenSession],
    tick: u64,
    reduced_motion: bool,
) -> u64 {
    canonical_tick_with_session_homes(height, width, sessions, tick, reduced_motion, false)
}

fn canonical_tick_with_session_homes(
    height: usize,
    width: usize,
    sessions: &[GardenSession],
    tick: u64,
    reduced_motion: bool,
    show_session_homes: bool,
) -> u64 {
    if reduced_motion {
        return 0;
    }
    let tick = tick % ANIMATION_CYCLE_TICKS;
    let expected = render_with_session_homes(
        height,
        width,
        "canonical",
        sessions,
        tick,
        false,
        show_session_homes,
    );
    let mut canonical = tick;
    // The sky changes every two ticks; only an immediately preceding identical
    // frame can be held. Include hitboxes so a moving click target also redraws.
    for distance in 1..=2 {
        let candidate = (tick + ANIMATION_CYCLE_TICKS - distance) % ANIMATION_CYCLE_TICKS;
        if render_with_session_homes(
            height,
            width,
            "canonical",
            sessions,
            candidate,
            false,
            show_session_homes,
        ) != expected
        {
            break;
        }
        canonical = candidate;
    }
    canonical
}

#[cfg(test)]
mod tests;
