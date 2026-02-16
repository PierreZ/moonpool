//! Dungeon workload for fork-based exploration testing.
//!
//! 8-floor roguelike with keys, monsters, rooms, health.
//! Each floor has a hidden key behind a P=0.03 probability gate.
//! Without exploration: P(reach floor 8) ~ (0.03)^7 ~ impossible.
//! With exploration: fork amplification at each floor.
//!
//! Ported from:
//! - Workload: `/home/pierrez/workspace/rust/Claude-fork-testing/maze-explorer/src/dungeon.rs`
//! - Engine: `/home/pierrez/workspace/rust/Claude-fork-testing/dungeon/src/lib.rs`

use moonpool_sim::{ExplorationConfig, SimulationBuilder, SimulationReport};

// ============================================================================
// Inlined dungeon game engine
// ============================================================================

#[allow(dead_code)]
mod dungeon_game {
    use rand::RngExt;

    /// Grid width in tiles.
    pub const WIDTH: usize = 35;
    /// Grid height in tiles.
    pub const HEIGHT: usize = 20;

    const MIN_ROOMS: usize = 4;
    const MAX_ROOMS: usize = 6;
    const MIN_ROOM_W: usize = 3;
    const MAX_ROOM_W: usize = 6;
    const MIN_ROOM_H: usize = 3;
    const MAX_ROOM_H: usize = 5;
    const MIN_ENEMIES: usize = 2;
    const MAX_ENEMIES: usize = 5;

    pub type Pos = (usize, usize);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Tile {
        Wall,
        Floor,
        StairsDown,
        Treasure,
        Potion,
        Trap,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Action {
        Move(i8, i8),
        DownStairs,
        Search,
    }

    impl Action {
        pub fn from_index(i: u8) -> Self {
            match i {
                0 => Action::Move(0, -1),  // N
                1 => Action::Move(1, -1),  // NE
                2 => Action::Move(1, 0),   // E
                3 => Action::Move(1, 1),   // SE
                4 => Action::Move(0, 1),   // S
                5 => Action::Move(-1, 1),  // SW
                6 => Action::Move(-1, 0),  // W
                7 => Action::Move(-1, -1), // NW
                8 => Action::DownStairs,
                _ => Action::Search,
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum StepOutcome {
        Alive,
        Dead,
        Descended,
        Won,
        MissingKey,
        Damaged,
        Healed,
        EnteredRoom,
    }

    #[derive(Clone, Debug)]
    pub struct Status {
        pub level: u32,
        pub alive: bool,
        pub has_key: bool,
        pub position: Pos,
        pub hp: i32,
    }

    struct Room {
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    }

    impl Room {
        fn center(&self) -> Pos {
            (self.x + self.w / 2, self.y + self.h / 2)
        }
    }

    fn generate_level(
        rng: &mut impl rand::Rng,
        level: u32,
        target_level: u32,
    ) -> (Vec<Tile>, Pos, Vec<Pos>, Option<Pos>, Vec<u8>, u8) {
        let mut grid = vec![Tile::Wall; WIDTH * HEIGHT];
        let mut room_map = vec![u8::MAX; WIDTH * HEIGHT];

        let num_rooms = rng.random_range(MIN_ROOMS..=MAX_ROOMS);
        let mut rooms = Vec::with_capacity(num_rooms);

        for _ in 0..num_rooms {
            let w = rng.random_range(MIN_ROOM_W..=MAX_ROOM_W);
            let h = rng.random_range(MIN_ROOM_H..=MAX_ROOM_H);
            let x = rng.random_range(1..WIDTH.saturating_sub(w + 1).max(2));
            let y = rng.random_range(1..HEIGHT.saturating_sub(h + 1).max(2));

            for ry in y..y + h {
                for rx in x..x + w {
                    if rx < WIDTH && ry < HEIGHT {
                        grid[ry * WIDTH + rx] = Tile::Floor;
                    }
                }
            }
            rooms.push(Room { x, y, w, h });
        }

        rooms.sort_by_key(|r| r.center().0);

        for (room_idx, room) in rooms.iter().enumerate() {
            for ry in room.y..room.y + room.h {
                for rx in room.x..room.x + room.w {
                    if rx < WIDTH && ry < HEIGHT {
                        room_map[ry * WIDTH + rx] = room_idx as u8;
                    }
                }
            }
        }

        for i in 0..rooms.len() - 1 {
            let (x1, y1) = rooms[i].center();
            let (x2, y2) = rooms[i + 1].center();

            let (lx, rx) = if x1 < x2 { (x1, x2) } else { (x2, x1) };
            for x in lx..=rx {
                grid[y1 * WIDTH + x] = Tile::Floor;
            }
            let (ly, ry) = if y1 < y2 { (y1, y2) } else { (y2, y1) };
            for y in ly..=ry {
                grid[y * WIDTH + x2] = Tile::Floor;
            }
        }

        let player = rooms[0].center();

        if level < target_level {
            let stairs = rooms[rooms.len() - 1].center();
            grid[stairs.1 * WIDTH + stairs.0] = Tile::StairsDown;
        }

        if level == target_level {
            let treasure = rooms[rooms.len() - 1].center();
            grid[treasure.1 * WIDTH + treasure.0] = Tile::Treasure;
        }

        let key_pos = if level < target_level && rooms.len() > 2 {
            let key_room_idx = rng.random_range(1..rooms.len() - 1);
            Some(rooms[key_room_idx].center())
        } else if level < target_level {
            Some(rooms[0].center())
        } else {
            None
        };

        if level < target_level && rooms.len() > 2 {
            let potion_room_idx = rng.random_range(1..rooms.len() - 1);
            let potion_pos = rooms[potion_room_idx].center();
            if key_pos != Some(potion_pos) {
                grid[potion_pos.1 * WIDTH + potion_pos.0] = Tile::Potion;
            }
        }

        let num_traps = rng.random_range(2..=3);
        for _ in 0..num_traps {
            for _ in 0..100 {
                let tx = rng.random_range(0..WIDTH);
                let ty = rng.random_range(0..HEIGHT);
                if grid[ty * WIDTH + tx] == Tile::Floor && (tx, ty) != player {
                    grid[ty * WIDTH + tx] = Tile::Trap;
                    break;
                }
            }
        }

        let min_enemies = MIN_ENEMIES + (level as usize) / 2;
        let max_enemies = MAX_ENEMIES + (level as usize) / 2;
        let num_enemies = rng.random_range(min_enemies..=max_enemies);
        let mut enemies = Vec::with_capacity(num_enemies);
        for _ in 0..num_enemies {
            for _ in 0..100 {
                let ex = rng.random_range(0..WIDTH);
                let ey = rng.random_range(0..HEIGHT);
                if grid[ey * WIDTH + ex] == Tile::Floor && (ex, ey) != player {
                    enemies.push((ex, ey));
                    break;
                }
            }
        }

        (grid, player, enemies, key_pos, room_map, rooms.len() as u8)
    }

    /// Dungeon game state.
    pub struct Game {
        grid: Vec<Tile>,
        room_map: Vec<u8>,
        num_rooms: u8,
        current_room: u8,
        pub player: Pos,
        enemies: Vec<Pos>,
        alive: bool,
        level: u32,
        target_level: u32,
        has_key: bool,
        key_pos: Option<Pos>,
        hp: i32,
        max_hp: i32,
    }

    impl Game {
        pub fn new(rng: &mut impl rand::Rng, target_level: u32) -> Self {
            let (grid, player, enemies, key_pos, room_map, num_rooms) =
                generate_level(rng, 1, target_level);
            let current_room = room_map[player.1 * WIDTH + player.0];
            Game {
                grid,
                room_map,
                num_rooms,
                current_room,
                player,
                enemies,
                alive: true,
                level: 1,
                target_level,
                has_key: false,
                key_pos,
                hp: 5,
                max_hp: 5,
            }
        }

        pub fn act(&mut self, action: Action, rng: &mut impl rand::Rng) -> StepOutcome {
            if !self.alive {
                return StepOutcome::Dead;
            }

            let mut entered_room = false;

            match action {
                Action::Move(dx, dy) => {
                    let nx = self.player.0 as i32 + dx as i32;
                    let ny = self.player.1 as i32 + dy as i32;
                    if nx >= 0 && nx < WIDTH as i32 && ny >= 0 && ny < HEIGHT as i32 {
                        let nu = (nx as usize, ny as usize);
                        if self.grid[nu.1 * WIDTH + nu.0] != Tile::Wall {
                            self.player = nu;
                        }
                    }

                    let new_room = self.room_map[self.player.1 * WIDTH + self.player.0];
                    if new_room != self.current_room && new_room < self.num_rooms {
                        entered_room = true;
                    }
                    self.current_room = new_room;

                    let tile = self.grid[self.player.1 * WIDTH + self.player.0];
                    match tile {
                        Tile::Potion => {
                            self.hp = (self.hp + 2).min(self.max_hp);
                            self.grid[self.player.1 * WIDTH + self.player.0] = Tile::Floor;
                        }
                        Tile::Trap => {
                            self.hp -= 1;
                            self.grid[self.player.1 * WIDTH + self.player.0] = Tile::Floor;
                            if self.hp <= 0 {
                                self.alive = false;
                                return StepOutcome::Dead;
                            }
                        }
                        _ => {}
                    }
                }
                Action::DownStairs => {
                    if self.grid[self.player.1 * WIDTH + self.player.0] == Tile::StairsDown {
                        if !self.has_key {
                            return StepOutcome::MissingKey;
                        }
                        self.level += 1;
                        self.has_key = false;
                        let (grid, player, enemies, key_pos, room_map, num_rooms) =
                            generate_level(rng, self.level, self.target_level);
                        self.grid = grid;
                        self.room_map = room_map;
                        self.num_rooms = num_rooms;
                        self.current_room = self.room_map[player.1 * WIDTH + player.0];
                        self.player = player;
                        self.enemies = enemies;
                        self.key_pos = key_pos;
                        return StepOutcome::Descended;
                    }
                }
                Action::Search => {
                    if self.grid[self.player.1 * WIDTH + self.player.0] == Tile::Treasure {
                        self.alive = false;
                        return StepOutcome::Won;
                    }
                }
            }

            // Move enemies randomly.
            for i in 0..self.enemies.len() {
                let (ex, ey) = self.enemies[i];
                let dir: u8 = rng.random_range(0..9);
                let (dx, dy): (i32, i32) = match dir {
                    0 => (0, -1),
                    1 => (1, -1),
                    2 => (1, 0),
                    3 => (1, 1),
                    4 => (0, 1),
                    5 => (-1, 1),
                    6 => (-1, 0),
                    7 => (-1, -1),
                    _ => (0, 0),
                };
                let nx = ex as i32 + dx;
                let ny = ey as i32 + dy;
                if nx >= 0 && nx < WIDTH as i32 && ny >= 0 && ny < HEIGHT as i32 {
                    let nu = (nx as usize, ny as usize);
                    if self.grid[nu.1 * WIDTH + nu.0] != Tile::Wall {
                        self.enemies[i] = nu;
                    }
                }
            }

            // Check enemy contact.
            for &(ex, ey) in &self.enemies {
                if (ex, ey) == self.player {
                    let damage = rng.random_range(1..=2_i32);
                    self.hp -= damage;
                    if self.hp <= 0 {
                        self.alive = false;
                        return StepOutcome::Dead;
                    }
                    return StepOutcome::Damaged;
                }
            }

            if entered_room {
                StepOutcome::EnteredRoom
            } else {
                StepOutcome::Alive
            }
        }

        pub fn player_tile(&self) -> Tile {
            self.grid[self.player.1 * WIDTH + self.player.0]
        }

        pub fn on_key_tile(&self) -> bool {
            matches!(self.key_pos, Some(kp) if kp == self.player)
        }

        pub fn has_key(&self) -> bool {
            self.has_key
        }

        pub fn grant_key(&mut self) {
            self.has_key = true;
        }

        pub fn hp(&self) -> i32 {
            self.hp
        }

        pub fn player_room(&self) -> Option<u8> {
            if self.current_room < self.num_rooms {
                Some(self.current_room)
            } else {
                None
            }
        }

        pub fn num_rooms(&self) -> u8 {
            self.num_rooms
        }

        pub fn status(&self) -> Status {
            Status {
                level: self.level,
                alive: self.alive,
                has_key: self.has_key,
                position: self.player,
                hp: self.hp,
            }
        }
    }
}

// ============================================================================
// SimRngAdapter — routes randomness through moonpool-sim's tracked RNG
// ============================================================================

/// RNG adapter that delegates to moonpool-sim's thread-local simulation RNG.
/// This ensures all randomness (including level generation and enemy movement)
/// goes through the tracked RNG, making replay via breakpoints work.
struct SimRngAdapter;

impl rand::TryRng for SimRngAdapter {
    type Error = core::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok(moonpool_sim::sim_random::<u32>())
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        Ok(moonpool_sim::sim_random::<u64>())
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
        for chunk in dest.chunks_mut(8) {
            let val = moonpool_sim::sim_random::<u64>().to_le_bytes();
            let len = chunk.len();
            chunk.copy_from_slice(&val[..len]);
        }
        Ok(())
    }
}

// ============================================================================
// Dungeon wrapper — adapts Game to step-based execution with assertions
// ============================================================================

use dungeon_game::{Action, StepOutcome, Tile};

const DEFAULT_MAX_STEPS: u64 = 10000;
const DEFAULT_TARGET_LEVEL: u32 = 8;

/// Level-dependent assertion messages for key discovery events.
const KEY_FOUND_MSGS: [&str; 9] = [
    "key found L0",
    "key found L1",
    "key found L2",
    "key found L3",
    "key found L4",
    "key found L5",
    "key found L6",
    "key found L7",
    "key found L8",
];

/// Probability of finding the hidden key when on the key tile.
const KEY_FIND_P: f64 = 0.03;

/// Probability of repeating the previous action (structured rollout).
const REPEAT_ACTION_P: f64 = 0.70;

/// Result of a single dungeon step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepResult {
    Continue,
    BugFound,
    Done,
}

struct Dungeon {
    step_count: u64,
    max_steps: u64,
    game: dungeon_game::Game,
    last_action_index: Option<u8>,
}

impl Dungeon {
    fn new(max_steps: u64, target_level: u32) -> Self {
        let mut rng = SimRngAdapter;
        Dungeon {
            step_count: 0,
            max_steps,
            game: dungeon_game::Game::new(&mut rng, target_level),
            last_action_index: None,
        }
    }

    fn random_bool(p: f64) -> bool {
        moonpool_sim::sim_random::<f64>() < p
    }

    fn step(&mut self) -> StepResult {
        self.step_count += 1;

        // Pick action with structured rollout (correlated sequences).
        // Always consume both RNG values for consistent call count per step.
        let fresh_action_index = (moonpool_sim::sim_random::<u64>() % 10) as u8;
        let repeat = Self::random_bool(REPEAT_ACTION_P);
        let action_index = match (repeat, self.last_action_index) {
            (true, Some(last)) => last,
            _ => fresh_action_index,
        };
        self.last_action_index = Some(action_index);
        let action = Action::from_index(action_index);
        let level = self.game.status().level as u8;
        let has_key = self.game.has_key() as u8;
        let hp = self.game.hp();
        let hp_bucket: u8 = match hp {
            ..=1 => 0,
            2 => 1,
            3..=4 => 2,
            _ => 3,
        };

        // Execute the action.
        let mut rng = SimRngAdapter;
        let outcome = self.game.act(action, &mut rng);

        // Recompute HP bucket after action (may have changed from damage/healing).
        let hp_after = self.game.hp();
        let hp_bucket_after: u8 = match hp_after {
            ..=1 => 0,
            2 => 1,
            3..=4 => 2,
            _ => 3,
        };

        // --- Key probability gate ---
        if self.game.on_key_tile() && !self.game.has_key() {
            moonpool_sim::assert_sometimes_each!(
                "on key tile",
                [("floor", level as i64)],
                [("health", hp_bucket as i64)]
            );
            if Self::random_bool(KEY_FIND_P) {
                self.game.grant_key();
                let lvl = level as usize;
                if lvl < KEY_FOUND_MSGS.len() {
                    moonpool_sim::assert_sometimes!(true, KEY_FOUND_MSGS[lvl]);
                }
            }
        }

        // Fork point when on stairs with key — descent amplification.
        if self.game.player_tile() == Tile::StairsDown && self.game.has_key() {
            moonpool_sim::assert_sometimes_each!(
                "stairs with key",
                [("floor", level as i64)],
                [("health", hp_bucket as i64)]
            );
        }

        // Fork point near treasure.
        if self.game.player_tile() == Tile::Treasure {
            moonpool_sim::assert_sometimes!(true, "near treasure");
        }

        match outcome {
            StepOutcome::Descended => {
                let new_level = self.game.status().level;
                let hp_bucket_desc: u8 = match self.game.hp() {
                    ..=1 => 0,
                    2 => 1,
                    3..=4 => 2,
                    _ => 3,
                };
                moonpool_sim::assert_sometimes_each!(
                    "descended",
                    [("to_floor", new_level as i64)],
                    [("health", hp_bucket_desc as i64)]
                );
            }
            StepOutcome::Won => {
                moonpool_sim::assert_sometimes!(true, "treasure found");
            }
            StepOutcome::MissingKey => {
                moonpool_sim::assert_sometimes!(true, "stairs without key");
            }
            StepOutcome::Damaged => {
                moonpool_sim::assert_sometimes!(true, "survived monster hit");
            }
            StepOutcome::Healed => {
                moonpool_sim::assert_sometimes!(true, "picked up potion");
            }
            StepOutcome::EnteredRoom => {
                if let Some(room_idx) = self.game.player_room() {
                    moonpool_sim::assert_sometimes_each!(
                        "room explored",
                        [("floor", level as i64), ("room", room_idx as i64)],
                        [
                            ("has_key", has_key as i64),
                            ("health", hp_bucket_after as i64)
                        ]
                    );
                    if room_idx == self.game.num_rooms() - 1 {
                        moonpool_sim::assert_sometimes_each!(
                            "goal room",
                            [("floor", level as i64)],
                            [
                                ("has_key", has_key as i64),
                                ("health", hp_bucket_after as i64)
                            ]
                        );
                    }
                }
            }
            StepOutcome::Dead | StepOutcome::Alive => {}
        }

        // Track level and HP watermarks for guidance.
        moonpool_sim::assert_sometimes_greater_than!(
            self.game.status().level as i64,
            0,
            "dungeon level reached"
        );
        moonpool_sim::assert_sometimes_greater_than!(hp_after as i64, 0, "health remaining");

        match outcome {
            StepOutcome::Won => StepResult::BugFound,
            StepOutcome::Dead => StepResult::Done,
            StepOutcome::Alive
            | StepOutcome::Descended
            | StepOutcome::MissingKey
            | StepOutcome::Damaged
            | StepOutcome::Healed
            | StepOutcome::EnteredRoom => {
                if self.step_count >= self.max_steps {
                    StepResult::Done
                } else {
                    StepResult::Continue
                }
            }
        }
    }

    fn run(&mut self) -> StepResult {
        loop {
            match self.step() {
                StepResult::Continue => {}
                result => return result,
            }
        }
    }
}

/// Helper to run a simulation and return the report.
fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move { builder.run().await })
}

/// Dungeon exploration test with fork-based amplification.
///
/// With 7 key gates at P=0.03 each, brute force probability of reaching
/// floor 8 is ~ (0.03)^7 ~ 2.2e-11. Fork amplification at each floor
/// makes this reachable.
#[test]
fn slow_simulation_dungeon() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(ExplorationConfig {
                max_depth: 120,
                timelines_per_split: 4,
                global_energy: 2_000_000,
                adaptive: Some(moonpool_sim::AdaptiveConfig {
                    batch_size: 30,
                    min_timelines: 800,
                    max_timelines: 2_000,
                    per_mark_energy: 20_000,
                }),
            })
            .workload_fn("dungeon", |_ctx| async move {
                let mut dungeon = Dungeon::new(DEFAULT_MAX_STEPS, DEFAULT_TARGET_LEVEL);
                let result = dungeon.run();

                if result == StepResult::BugFound {
                    return Err(moonpool_sim::SimulationError::InvalidState(
                        "dungeon bug: treasure found on floor 8".to_string(),
                    ));
                }

                Ok(())
            }),
    );

    // Parent run should succeed
    assert_eq!(report.successful_runs, 1);

    let exp = report.exploration.expect("exploration report missing");
    assert!(exp.total_timelines > 0, "expected forked timelines, got 0");
    assert!(exp.bugs_found > 0, "exploration should have found the bug");
}

/// Test that a dungeon bug found by exploration can be replayed deterministically.
///
/// Same workflow as the maze replay test but for the more complex dungeon:
/// 1. Exploration finds a path through all 7 key gates to reach the treasure
/// 2. The recipe is captured and formatted as a timeline string
/// 3. Replaying with the same seed + RNG breakpoints reproduces the treasure find
#[test]
fn slow_simulation_dungeon_bug_replay() {
    // Phase 1: Run exploration to find the bug and capture the recipe.
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .set_debug_seeds(vec![54321])
            .enable_exploration(ExplorationConfig {
                max_depth: 120,
                timelines_per_split: 4,
                global_energy: 2_000_000,
                adaptive: Some(moonpool_sim::AdaptiveConfig {
                    batch_size: 30,
                    min_timelines: 800,
                    max_timelines: 2_000,
                    per_mark_energy: 20_000,
                }),
            })
            .workload_fn("dungeon", |_ctx| async move {
                let mut dungeon = Dungeon::new(DEFAULT_MAX_STEPS, DEFAULT_TARGET_LEVEL);
                let result = dungeon.run();

                if result == StepResult::BugFound {
                    return Err(moonpool_sim::SimulationError::InvalidState(
                        "dungeon bug: treasure found on floor 8".to_string(),
                    ));
                }

                Ok(())
            }),
    );

    assert_eq!(report.successful_runs, 1);

    let exp = report.exploration.expect("exploration report missing");
    assert!(exp.bugs_found > 0, "exploration should have found the bug");
    let recipe = exp.bug_recipe.expect("bug recipe should be captured");
    let initial_seed = report.seeds_used[0];

    // Phase 2: Format and parse the recipe (simulates developer copy-pasting from logs).
    let timeline_str = moonpool_sim::format_timeline(&recipe);
    eprintln!(
        "Replaying dungeon bug: seed={}, recipe={}",
        initial_seed, timeline_str
    );
    let parsed_recipe =
        moonpool_sim::parse_timeline(&timeline_str).expect("recipe should round-trip");
    assert_eq!(recipe, parsed_recipe);

    // Phase 3: Replay — same seed, breakpoints from recipe, no exploration.
    moonpool_sim::reset_sim_rng();
    moonpool_sim::set_sim_seed(initial_seed);
    moonpool_sim::set_rng_breakpoints(parsed_recipe);

    let mut dungeon = Dungeon::new(DEFAULT_MAX_STEPS, DEFAULT_TARGET_LEVEL);
    let result = dungeon.run();

    assert_eq!(
        result,
        StepResult::BugFound,
        "replaying the recipe should reproduce the dungeon bug"
    );
}
