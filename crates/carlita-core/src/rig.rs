//! The rig: several tracks that play at the same time, each one a chain of
//! plugins that may contain parallel blocks.
//!
//! ```text
//!            ┌──────────── track "Guitar" ─────────────┐
//! input ──┬─►│ comp ─► ┌ lane A: amp ──────┐ ─► reverb │─► bus ─┬─► output
//!         │  │         └ lane B: (dry) ────┘           │        │
//!         │  └─────────────────────────────────────────┘        │
//!         └─►  track "Vocals": eq ─► delay ──────────────► bus ─┘
//! ```
//!
//! Every track takes the rig input (both channels, or only the left or right
//! one) and ends in its own bus, which sets the track volume and feeds the
//! rig output. A parallel block sends its input into every lane and sums the
//! lanes' outputs; an empty lane is a dry path. Parallel blocks hold plain
//! plugins only, so the structure stays readable on a touchscreen.
//!
//! [`route`] turns the rig into a list of [`Link`]s that the engine wires up
//! in Carla's patchbay.

use crate::chain::{Chain, ChainSlot, SlotId};
use crate::plugin::PluginDescriptor;
use serde::{Deserialize, Serialize};

/// Stable identity of a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrackId(pub u64);

/// Which channels of the rig input feed a track.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackInput {
    /// Both input channels.
    #[default]
    Stereo,
    /// Only input 1, sent to both sides of a stereo plugin.
    Left,
    /// Only input 2, sent to both sides of a stereo plugin.
    Right,
    /// No audio input (a synth played over MIDI).
    None,
}

impl TrackInput {
    pub const ALL: [TrackInput; 4] = [Self::Stereo, Self::Left, Self::Right, Self::None];

    pub fn label(self) -> &'static str {
        match self {
            Self::Stereo => "In 1+2",
            Self::Left => "In 1",
            Self::Right => "In 2",
            Self::None => "No input",
        }
    }

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|x| *x == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }
}

/// Lanes of a parallel block.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Lane {
    #[serde(default)]
    pub slots: Vec<ChainSlot>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Parallel {
    pub lanes: Vec<Lane>,
}

/// One step of a track's chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Block {
    Plugin(ChainSlot),
    Parallel(Parallel),
}

fn unity() -> f32 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub name: String,
    #[serde(default)]
    pub input: TrackInput,
    /// Linear gain of the track bus, 0..=4 (unity is 1).
    #[serde(default = "unity")]
    pub volume: f32,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub blocks: Vec<Block>,
}

impl Track {
    fn new(id: TrackId, name: String) -> Self {
        Self {
            id,
            name,
            input: TrackInput::Stereo,
            volume: 1.0,
            muted: false,
            blocks: Vec::new(),
        }
    }

    /// Every plugin of the track, in signal order (lanes top to bottom).
    pub fn slots(&self) -> impl Iterator<Item = &ChainSlot> {
        self.blocks
            .iter()
            .flat_map(|b| -> Box<dyn Iterator<Item = &ChainSlot>> {
                match b {
                    Block::Plugin(s) => Box::new(std::iter::once(s)),
                    Block::Parallel(p) => Box::new(p.lanes.iter().flat_map(|l| l.slots.iter())),
                }
            })
    }

    fn slots_mut(&mut self) -> impl Iterator<Item = &mut ChainSlot> {
        self.blocks
            .iter_mut()
            .flat_map(|b| -> Box<dyn Iterator<Item = &mut ChainSlot>> {
                match b {
                    Block::Plugin(s) => Box::new(std::iter::once(s)),
                    Block::Parallel(p) => {
                        Box::new(p.lanes.iter_mut().flat_map(|l| l.slots.iter_mut()))
                    }
                }
            })
    }

    pub fn is_empty(&self) -> bool {
        self.slots().next().is_none()
    }

    /// Turns parallel blocks with a single lane back into serial plugins and
    /// drops blocks without lanes.
    fn normalize(&mut self) {
        let mut out = Vec::with_capacity(self.blocks.len());
        for b in std::mem::take(&mut self.blocks) {
            match b {
                Block::Parallel(p) if p.lanes.len() <= 1 => {
                    for l in p.lanes {
                        out.extend(l.slots.into_iter().map(Block::Plugin));
                    }
                }
                b => out.push(b),
            }
        }
        self.blocks = out;
    }
}

/// Where a plugin sits (or is to be inserted) inside a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spot {
    /// As its own block at this index of the track.
    Serial { block: usize },
    /// Inside lane `lane` of the parallel block `block`, at `index`.
    Lane {
        block: usize,
        lane: usize,
        index: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub track: TrackId,
    pub spot: Spot,
}

/// All tracks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rig {
    pub tracks: Vec<Track>,
    #[serde(default)]
    next_id: u64,
}

impl Rig {
    /// A rig with one empty track.
    pub fn new() -> Self {
        let mut r = Self::default();
        r.ensure_track();
        r
    }

    /// Turns a preset from before tracks existed into a one-track rig.
    pub fn from_chain(chain: Chain) -> Self {
        let mut r = Self::new();
        r.tracks[0].blocks = chain.slots.into_iter().map(Block::Plugin).collect();
        r
    }

    fn allocate(&mut self) -> u64 {
        let max = self
            .tracks
            .iter()
            .flat_map(|t| std::iter::once(t.id.0).chain(t.slots().map(|s| s.id.0)))
            .map(|id| id + 1)
            .max()
            .unwrap_or(0);
        self.next_id = self.next_id.max(max);
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Makes sure there is at least one track and returns the first one.
    pub fn ensure_track(&mut self) -> TrackId {
        if self.tracks.is_empty() {
            self.add_track(None);
        }
        self.tracks[0].id
    }

    /// Adds a track at the end; without a name it is called "Track N".
    pub fn add_track(&mut self, name: Option<String>) -> TrackId {
        let id = TrackId(self.allocate());
        let name = name.unwrap_or_else(|| {
            (1..)
                .map(|n| format!("Track {n}"))
                .find(|n| !self.tracks.iter().any(|t| &t.name == n))
                .expect("unbounded")
        });
        self.tracks.push(Track::new(id, name));
        id
    }

    pub fn remove_track(&mut self, id: TrackId) -> Option<(usize, Track)> {
        let i = self.tracks.iter().position(|t| t.id == id)?;
        Some((i, self.tracks.remove(i)))
    }

    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub fn track_mut(&mut self, id: TrackId) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }

    /// Creates a slot for `plugin` (not yet inserted).
    pub fn new_slot(&mut self, plugin: PluginDescriptor) -> ChainSlot {
        ChainSlot {
            id: SlotId(self.allocate()),
            name: plugin.name.clone(),
            plugin,
            bypassed: false,
            scaling: None,
            state: None,
        }
    }

    pub fn slots(&self) -> impl Iterator<Item = &ChainSlot> {
        self.tracks.iter().flat_map(|t| t.slots())
    }

    pub fn slot_count(&self) -> usize {
        self.slots().count()
    }

    pub fn is_empty(&self) -> bool {
        self.slots().next().is_none()
    }

    pub fn get(&self, id: SlotId) -> Option<&ChainSlot> {
        self.slots().find(|s| s.id == id)
    }

    pub fn get_mut(&mut self, id: SlotId) -> Option<&mut ChainSlot> {
        self.tracks
            .iter_mut()
            .flat_map(|t| t.slots_mut())
            .find(|s| s.id == id)
    }

    /// The track that holds `id`.
    pub fn track_of(&self, id: SlotId) -> Option<TrackId> {
        self.locate(id).map(|l| l.track)
    }

    pub fn locate(&self, id: SlotId) -> Option<Location> {
        for t in &self.tracks {
            for (b, block) in t.blocks.iter().enumerate() {
                match block {
                    Block::Plugin(s) if s.id == id => {
                        return Some(Location {
                            track: t.id,
                            spot: Spot::Serial { block: b },
                        })
                    }
                    Block::Plugin(_) => {}
                    Block::Parallel(p) => {
                        for (l, lane) in p.lanes.iter().enumerate() {
                            if let Some(i) = lane.slots.iter().position(|s| s.id == id) {
                                return Some(Location {
                                    track: t.id,
                                    spot: Spot::Lane {
                                        block: b,
                                        lane: l,
                                        index: i,
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Removes a plugin and returns where it was. A parallel block whose
    /// lanes all end up empty is removed too.
    pub fn take(&mut self, id: SlotId) -> Option<(Location, ChainSlot)> {
        let loc = self.locate(id)?;
        let slot = self.take_raw(loc)?;
        self.prune(loc.track);
        Some((loc, slot))
    }

    fn take_raw(&mut self, loc: Location) -> Option<ChainSlot> {
        let t = self.track_mut(loc.track)?;
        Some(match loc.spot {
            Spot::Serial { block } => match t.blocks.remove(block) {
                Block::Plugin(s) => s,
                Block::Parallel(_) => unreachable!("locate returned a parallel block"),
            },
            Spot::Lane { block, lane, index } => {
                let Block::Parallel(p) = &mut t.blocks[block] else {
                    unreachable!("locate returned a lane of a serial block")
                };
                p.lanes[lane].slots.remove(index)
            }
        })
    }

    /// Drops parallel blocks whose lanes are all empty: they would only add
    /// up copies of the dry signal.
    fn prune(&mut self, track: TrackId) {
        if let Some(t) = self.track_mut(track) {
            t.blocks.retain(
                |b| !matches!(b, Block::Parallel(p) if p.lanes.iter().all(|l| l.slots.is_empty())),
            );
        }
    }

    /// Inserts a plugin. Out-of-range positions are clamped; a lane that no
    /// longer exists falls back to a serial position, and a missing track to
    /// the first track.
    pub fn insert(&mut self, loc: Location, slot: ChainSlot) -> Location {
        let first = self.ensure_track();
        let track = if self.track(loc.track).is_some() {
            loc.track
        } else {
            first
        };
        let t = self.track_mut(track).expect("track exists");
        let n = t.blocks.len();
        let spot = match loc.spot {
            Spot::Lane { block, lane, index } => match t.blocks.get_mut(block) {
                Some(Block::Parallel(p)) if lane < p.lanes.len() => {
                    let lane_slots = &mut p.lanes[lane].slots;
                    let index = index.min(lane_slots.len());
                    lane_slots.insert(index, slot);
                    return Location {
                        track,
                        spot: Spot::Lane { block, lane, index },
                    };
                }
                _ => Spot::Serial {
                    block: block.min(n),
                },
            },
            Spot::Serial { block } => Spot::Serial {
                block: block.min(n),
            },
        };
        let Spot::Serial { block } = spot else {
            unreachable!()
        };
        t.blocks.insert(block, Block::Plugin(slot));
        Location { track, spot }
    }

    /// Appends a plugin to the end of a track.
    pub fn push(&mut self, track: TrackId, slot: ChainSlot) -> Location {
        let block = self.track(track).map_or(usize::MAX, |t| t.blocks.len());
        self.insert(
            Location {
                track,
                spot: Spot::Serial { block },
            },
            slot,
        )
    }

    /// Moves a plugin. `to` is given in the positions from before the move
    /// (as the UI sees them); a target after the plugin's own position in
    /// the same list is shifted to account for its removal.
    pub fn move_slot(&mut self, id: SlotId, to: Location) -> bool {
        let Some(from) = self.locate(id) else {
            return false;
        };
        let mut spot = to.spot;
        if from.track == to.track {
            match (from.spot, &mut spot) {
                (Spot::Serial { block: fb }, Spot::Serial { block: tb }) if *tb > fb => *tb -= 1,
                (Spot::Serial { block: fb }, Spot::Lane { block: tb, .. }) if *tb > fb => *tb -= 1,
                (
                    Spot::Lane {
                        block: fb,
                        lane: fl,
                        index: fi,
                    },
                    Spot::Lane {
                        block: tb,
                        lane: tl,
                        index: ti,
                    },
                ) if fb == *tb && fl == *tl && *ti > fi => *ti -= 1,
                _ => {}
            }
            if spot == from.spot {
                return false;
            }
        }
        // Keep an emptied parallel block until the plugin is back in, so the
        // target positions stay valid, then drop it.
        let Some(slot) = self.take_raw(from) else {
            return false;
        };
        self.insert(
            Location {
                track: to.track,
                spot,
            },
            slot,
        );
        self.prune(from.track);
        true
    }

    /// Runs a serial plugin in parallel with a dry lane.
    pub fn split(&mut self, id: SlotId) -> bool {
        let Some(Location {
            track,
            spot: Spot::Serial { block },
        }) = self.locate(id)
        else {
            return false;
        };
        let t = self.track_mut(track).expect("located");
        let Block::Plugin(slot) = t.blocks.remove(block) else {
            unreachable!()
        };
        t.blocks.insert(
            block,
            Block::Parallel(Parallel {
                lanes: vec![Lane { slots: vec![slot] }, Lane { slots: Vec::new() }],
            }),
        );
        true
    }

    /// Adds an empty (dry) lane to a parallel block.
    pub fn add_lane(&mut self, track: TrackId, block: usize) -> bool {
        match self.track_mut(track).and_then(|t| t.blocks.get_mut(block)) {
            Some(Block::Parallel(p)) => {
                p.lanes.push(Lane::default());
                true
            }
            _ => false,
        }
    }

    /// Removes a lane and returns its plugins. A block left with one lane
    /// becomes serial again.
    pub fn remove_lane(
        &mut self,
        track: TrackId,
        block: usize,
        lane: usize,
    ) -> Option<Vec<ChainSlot>> {
        let t = self.track_mut(track)?;
        let Some(Block::Parallel(p)) = t.blocks.get_mut(block) else {
            return None;
        };
        if lane >= p.lanes.len() {
            return None;
        }
        let removed = p.lanes.remove(lane).slots;
        if p.lanes.iter().all(|l| l.slots.is_empty()) {
            t.blocks.remove(block);
        } else {
            t.normalize();
        }
        Some(removed)
    }

    /// The structure the engine needs to wire the rig.
    pub fn layout(&self) -> RigLayout {
        RigLayout {
            tracks: self
                .tracks
                .iter()
                .map(|t| TrackLayout {
                    id: t.id,
                    input: t.input,
                    volume: t.volume,
                    muted: t.muted,
                    blocks: t
                        .blocks
                        .iter()
                        .map(|b| match b {
                            Block::Plugin(s) => BlockLayout::Slot(s.id),
                            Block::Parallel(p) => BlockLayout::Parallel(
                                p.lanes
                                    .iter()
                                    .map(|l| l.slots.iter().map(|s| s.id).collect())
                                    .collect(),
                            ),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

// ------------------------------------------------------------------ UI rows

/// What a row of the chain list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Plugin,
    /// Header of a parallel block.
    Parallel,
    /// Header of a lane.
    Lane,
    /// Placeholder row of an empty lane.
    Dry,
}

/// One row of a track's chain list, flattened for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    pub slot: Option<SlotId>,
    pub block: usize,
    pub lane: Option<usize>,
    /// Position within the lane.
    pub index: usize,
}

/// The rows of a track's chain list, top to bottom.
pub fn rows(track: &Track) -> Vec<Row> {
    let mut out = Vec::new();
    for (b, block) in track.blocks.iter().enumerate() {
        match block {
            Block::Plugin(s) => out.push(Row {
                kind: RowKind::Plugin,
                slot: Some(s.id),
                block: b,
                lane: None,
                index: 0,
            }),
            Block::Parallel(p) => {
                out.push(Row {
                    kind: RowKind::Parallel,
                    slot: None,
                    block: b,
                    lane: None,
                    index: 0,
                });
                for (l, lane) in p.lanes.iter().enumerate() {
                    out.push(Row {
                        kind: RowKind::Lane,
                        slot: None,
                        block: b,
                        lane: Some(l),
                        index: 0,
                    });
                    if lane.slots.is_empty() {
                        out.push(Row {
                            kind: RowKind::Dry,
                            slot: None,
                            block: b,
                            lane: Some(l),
                            index: 0,
                        });
                    }
                    for (i, s) in lane.slots.iter().enumerate() {
                        out.push(Row {
                            kind: RowKind::Plugin,
                            slot: Some(s.id),
                            block: b,
                            lane: Some(l),
                            index: i,
                        });
                    }
                }
            }
        }
    }
    out
}

/// Where a plugin dragged from row `from` and dropped on row `to` goes: it
/// takes the place of the row it is dropped on. `to == rows.len()` drops it
/// at the end of the track. The result is in pre-move positions, as
/// [`Rig::move_slot`] expects.
pub fn drop_spot(track: &Track, rows: &[Row], from: usize, to: usize) -> Option<Spot> {
    if from >= rows.len() || from == to || rows[from].kind != RowKind::Plugin {
        return None;
    }
    let down = to > from;
    let Some(r) = rows.get(to) else {
        return Some(Spot::Serial {
            block: track.blocks.len(),
        });
    };
    let lane_len = |b: usize, l: usize| match track.blocks.get(b) {
        Some(Block::Parallel(p)) => p.lanes.get(l).map_or(0, |l| l.slots.len()),
        _ => 0,
    };
    Some(match (r.kind, r.lane) {
        (RowKind::Plugin, None) => Spot::Serial {
            block: r.block + down as usize,
        },
        (RowKind::Plugin, Some(lane)) => Spot::Lane {
            block: r.block,
            lane,
            index: r.index + down as usize,
        },
        (RowKind::Parallel, _) if down => Spot::Lane {
            block: r.block,
            lane: 0,
            index: 0,
        },
        (RowKind::Parallel, _) => Spot::Serial { block: r.block },
        (RowKind::Lane, Some(lane)) if !down && lane > 0 => Spot::Lane {
            block: r.block,
            lane: lane - 1,
            index: lane_len(r.block, lane - 1),
        },
        (RowKind::Lane, Some(_)) if !down => Spot::Serial { block: r.block },
        (RowKind::Lane | RowKind::Dry, Some(lane)) => Spot::Lane {
            block: r.block,
            lane,
            index: 0,
        },
        (RowKind::Lane | RowKind::Dry, None) => return None,
    })
}

// ------------------------------------------------------------------ routing

/// The rig's structure without plugin details, for the engine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RigLayout {
    pub tracks: Vec<TrackLayout>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackLayout {
    pub id: TrackId,
    pub input: TrackInput,
    pub volume: f32,
    pub muted: bool,
    pub blocks: Vec<BlockLayout>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockLayout {
    Slot(SlotId),
    Parallel(Vec<Vec<SlotId>>),
}

impl RigLayout {
    pub fn slot_ids(&self) -> impl Iterator<Item = SlotId> + '_ {
        self.tracks.iter().flat_map(|t| {
            t.blocks
                .iter()
                .flat_map(|b| -> Box<dyn Iterator<Item = SlotId> + '_> {
                    match b {
                        BlockLayout::Slot(s) => Box::new(std::iter::once(*s)),
                        BlockLayout::Parallel(l) => Box::new(l.iter().flatten().copied()),
                    }
                })
        })
    }
}

/// A point in the signal graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Node {
    /// The rig's audio input (outputs of the input bus).
    Input,
    /// The rig's audio output (input of the output bus).
    Output,
    /// Hardware MIDI coming in.
    MidiInput,
    /// MIDI going out of the rig.
    MidiOutput,
    /// The bus at the end of a track.
    Track(TrackId),
    Slot(SlotId),
}

/// Which outputs of the source take part in a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortSel {
    All,
    /// Only the first output.
    Left,
    /// Only the second output.
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkKind {
    Audio,
    Midi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Link {
    pub kind: LinkKind,
    pub from: Node,
    pub sel: PortSel,
    pub to: Node,
}

/// Port counts of a running plugin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeIo {
    pub audio_ins: u32,
    pub audio_outs: u32,
    pub midi_ins: u32,
    pub midi_outs: u32,
}

#[derive(Clone)]
struct Frontier {
    audio: Vec<(Node, PortSel)>,
    midi: Vec<Node>,
}

fn union<T: PartialEq + Copy>(into: &mut Vec<T>, more: &[T]) {
    for x in more {
        if !into.contains(x) {
            into.push(*x);
        }
    }
}

/// Computes the connections that make the rig sound.
///
/// `io` gives the ports of each plugin, or `None` for plugins that are not
/// running (still loading, or bypassed by routing around them); those are
/// skipped. Like a pedalboard, a plugin without audio outputs (a tuner)
/// passes audio on untouched, and a plugin without MIDI outputs passes MIDI
/// on, so every plugin further down still gets it.
pub fn route(layout: &RigLayout, io: impl Fn(SlotId) -> Option<NodeIo>) -> Vec<Link> {
    let mut links: Vec<Link> = Vec::new();
    let mut add = |l: Link| {
        if !links.contains(&l) {
            links.push(l);
        }
    };
    let step = |f: &mut Frontier, id: SlotId, add: &mut dyn FnMut(Link)| {
        let Some(p) = io(id) else { return };
        let node = Node::Slot(id);
        if p.audio_ins > 0 {
            for &(from, sel) in &f.audio {
                add(Link {
                    kind: LinkKind::Audio,
                    from,
                    sel,
                    to: node,
                });
            }
        }
        if p.audio_outs > 0 {
            f.audio = vec![(node, PortSel::All)];
        }
        if p.midi_ins > 0 {
            for &from in &f.midi {
                add(Link {
                    kind: LinkKind::Midi,
                    from,
                    sel: PortSel::All,
                    to: node,
                });
            }
        }
        if p.midi_outs > 0 {
            f.midi = vec![node];
        }
    };
    for t in &layout.tracks {
        let mut f = Frontier {
            audio: match t.input {
                TrackInput::Stereo => vec![(Node::Input, PortSel::All)],
                TrackInput::Left => vec![(Node::Input, PortSel::Left)],
                TrackInput::Right => vec![(Node::Input, PortSel::Right)],
                TrackInput::None => vec![],
            },
            midi: vec![Node::MidiInput],
        };
        for b in &t.blocks {
            match b {
                BlockLayout::Slot(id) => step(&mut f, *id, &mut add),
                BlockLayout::Parallel(lanes) => {
                    let start = f.clone();
                    let mut end = Frontier {
                        audio: Vec::new(),
                        midi: Vec::new(),
                    };
                    for lane in lanes {
                        let mut lf = start.clone();
                        for id in lane {
                            step(&mut lf, *id, &mut add);
                        }
                        union(&mut end.audio, &lf.audio);
                        union(&mut end.midi, &lf.midi);
                    }
                    if !lanes.is_empty() {
                        f = end;
                    }
                }
            }
        }
        let bus = Node::Track(t.id);
        for &(from, sel) in &f.audio {
            add(Link {
                kind: LinkKind::Audio,
                from,
                sel,
                to: bus,
            });
        }
        if !t.muted {
            add(Link {
                kind: LinkKind::Audio,
                from: bus,
                sel: PortSel::All,
                to: Node::Output,
            });
        }
        for &from in &f.midi {
            if from != Node::MidiInput {
                add(Link {
                    kind: LinkKind::Midi,
                    from,
                    sel: PortSel::All,
                    to: Node::MidiOutput,
                });
            }
        }
    }
    links
}

/// The longest plugin delay through the rig, in samples: lanes of a
/// parallel block take the longest lane, tracks the longest track.
pub fn path_latency(layout: &RigLayout, latency: impl Fn(SlotId) -> u32) -> u32 {
    layout
        .tracks
        .iter()
        .map(|t| {
            t.blocks
                .iter()
                .map(|b| match b {
                    BlockLayout::Slot(s) => latency(*s),
                    BlockLayout::Parallel(lanes) => lanes
                        .iter()
                        .map(|l| l.iter().map(|s| latency(*s)).sum::<u32>())
                        .max()
                        .unwrap_or(0),
                })
                .sum::<u32>()
        })
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{PluginCategory, PluginFormat};

    fn desc(name: &str) -> PluginDescriptor {
        PluginDescriptor {
            format: PluginFormat::Internal,
            arch: Default::default(),
            name: name.into(),
            label: name.into(),
            maker: String::new(),
            filename: String::new(),
            unique_id: 0,
            category: PluginCategory::Other,
            audio_ins: 2,
            audio_outs: 2,
            midi_ins: 0,
            midi_outs: 0,
            parameters: 0,
            is_synth: false,
            has_custom_ui: false,
            yabridge: false,
        }
    }

    /// A one-track rig with plugins named after `names`; returns their ids.
    fn rig(names: &[&str]) -> (Rig, TrackId, Vec<SlotId>) {
        let mut r = Rig::new();
        let t = r.tracks[0].id;
        let ids = names
            .iter()
            .map(|n| {
                let s = r.new_slot(desc(n));
                let id = s.id;
                r.push(t, s);
                id
            })
            .collect();
        (r, t, ids)
    }

    /// Compact text form of a track: `a [b c | ] d`.
    fn shape(r: &Rig, t: TrackId) -> String {
        let t = r.track(t).unwrap();
        t.blocks
            .iter()
            .map(|b| match b {
                Block::Plugin(s) => s.name.clone(),
                Block::Parallel(p) => format!(
                    "[{}]",
                    p.lanes
                        .iter()
                        .map(|l| l
                            .slots
                            .iter()
                            .map(|s| s.name.as_str())
                            .collect::<Vec<_>>()
                            .join(" "))
                        .collect::<Vec<_>>()
                        .join(" | ")
                ),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn drag(r: &mut Rig, t: TrackId, from: usize, to: usize) {
        let track = r.track(t).unwrap().clone();
        let rows = rows(&track);
        let spot = drop_spot(&track, &rows, from, to).expect("valid drop");
        let id = rows[from].slot.unwrap();
        r.move_slot(id, Location { track: t, spot });
    }

    #[test]
    fn serial_moves() {
        let (mut r, t, _) = rig(&["a", "b", "c", "d"]);
        drag(&mut r, t, 0, 2);
        assert_eq!(shape(&r, t), "b c a d");
        drag(&mut r, t, 3, 0);
        assert_eq!(shape(&r, t), "d b c a");
        drag(&mut r, t, 0, 4);
        assert_eq!(shape(&r, t), "b c a d");
    }

    #[test]
    fn split_and_move_into_lanes() {
        let (mut r, t, ids) = rig(&["a", "b", "c"]);
        assert!(r.split(ids[1]));
        assert_eq!(shape(&r, t), "a [b | ] c");
        // Rows: a, ‖, lane0, b, lane1, dry, c
        let kinds: Vec<RowKind> = rows(r.track(t).unwrap()).iter().map(|r| r.kind).collect();
        use RowKind::*;
        assert_eq!(kinds, [Plugin, Parallel, Lane, Plugin, Lane, Dry, Plugin]);
        // Drop "c" on the dry row of lane 1.
        drag(&mut r, t, 6, 5);
        assert_eq!(shape(&r, t), "a [b | c]");
        // Drop "a" on b (moving down) -> after b in lane 0.
        drag(&mut r, t, 0, 3);
        assert_eq!(shape(&r, t), "[b a | c]");
        // Rows: ‖, lane0, b, a, lane1, c. Drag "c" up onto lane1's header
        // -> end of lane 0; lane 1 becomes dry.
        drag(&mut r, t, 5, 4);
        assert_eq!(shape(&r, t), "[b a c | ]");
        // Drag "b" up onto the block header -> before the block.
        drag(&mut r, t, 2, 0);
        assert_eq!(shape(&r, t), "b [a c | ]");
    }

    #[test]
    fn emptying_a_parallel_block_removes_it() {
        let (mut r, t, ids) = rig(&["a", "b"]);
        r.split(ids[0]);
        // Moving the only lane plugin out removes the block, and the target
        // index accounts for that.
        drag(&mut r, t, 2, 5);
        assert_eq!(shape(&r, t), "b a");
        r.split(ids[1]);
        let (loc, _) = r.take(ids[1]).unwrap();
        assert_eq!(shape(&r, t), "a");
        // Undo puts it back as a serial plugin.
        let s = r.new_slot(desc("b2"));
        r.insert(loc, s);
        assert_eq!(shape(&r, t), "b2 a");
        // Moving a lone lane plugin to the dry lane of its own block works.
        r.split(ids[0]);
        assert_eq!(shape(&r, t), "b2 [a | ]");
        drag(&mut r, t, 3, 5);
        assert_eq!(shape(&r, t), "b2 [ | a]");
    }

    #[test]
    fn lanes_add_and_remove() {
        let (mut r, t, ids) = rig(&["a", "b"]);
        r.split(ids[0]);
        assert!(r.add_lane(t, 0));
        assert_eq!(shape(&r, t), "[a |  | ] b");
        assert_eq!(r.remove_lane(t, 0, 1).unwrap().len(), 0);
        assert_eq!(shape(&r, t), "[a | ] b");
        // Down to one lane: the block becomes serial again.
        r.remove_lane(t, 0, 1);
        assert_eq!(shape(&r, t), "a b");
    }

    #[test]
    fn ids_unique_across_tracks() {
        let (mut r, _, ids) = rig(&["a"]);
        let t2 = r.add_track(None);
        assert_eq!(r.track(t2).unwrap().name, "Track 2");
        let s = r.new_slot(desc("x"));
        assert!(!ids.contains(&s.id) && s.id.0 != t2.0);
        r.push(t2, s.clone());
        assert_eq!(r.track_of(s.id), Some(t2));
        let (_, removed) = r.remove_track(t2).unwrap();
        assert_eq!(removed.slots().count(), 1);
        assert!(r.get(s.id).is_none());
    }

    #[test]
    fn legacy_chain_and_serde() {
        let mut c = Chain::default();
        let s = c.new_slot(desc("a"));
        c.push(s);
        let r = Rig::from_chain(c);
        assert_eq!(r.tracks.len(), 1);
        assert_eq!(r.slot_count(), 1);
        let mut r2 = r.clone();
        let id = r2.slots().next().unwrap().id;
        r2.split(id);
        let json = serde_json::to_string(&r2).unwrap();
        let back: Rig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r2);
    }

    fn fx() -> Option<NodeIo> {
        Some(NodeIo {
            audio_ins: 2,
            audio_outs: 2,
            ..Default::default()
        })
    }

    fn audio(from: Node, to: Node) -> Link {
        Link {
            kind: LinkKind::Audio,
            from,
            sel: PortSel::All,
            to,
        }
    }

    #[test]
    fn routes_serial_parallel_and_tracks() {
        let (mut r, t, ids) = rig(&["a", "b", "c"]);
        r.split(ids[1]);
        let t2 = r.add_track(None);
        r.track_mut(t2).unwrap().input = TrackInput::Right;
        r.track_mut(t2).unwrap().muted = true;
        let links = route(&r.layout(), |_| fx());
        let (a, b, c) = (Node::Slot(ids[0]), Node::Slot(ids[1]), Node::Slot(ids[2]));
        let expect = [
            audio(Node::Input, a),
            audio(a, b),
            // b's lane and the dry lane both feed c.
            audio(b, c),
            audio(a, c),
            audio(c, Node::Track(t)),
            audio(Node::Track(t), Node::Output),
            // Track 2: right input straight into its (muted) bus.
            Link {
                kind: LinkKind::Audio,
                from: Node::Input,
                sel: PortSel::Right,
                to: Node::Track(t2),
            },
        ];
        assert_eq!(links, expect);
    }

    #[test]
    fn routes_around_missing_plugins_and_passes_midi() {
        let (r, t, ids) = rig(&["synth", "tuner", "off", "fx"]);
        let io = |id: SlotId| match id {
            _ if id == ids[0] => Some(NodeIo {
                audio_ins: 0,
                audio_outs: 2,
                midi_ins: 1,
                midi_outs: 0,
            }),
            _ if id == ids[1] => Some(NodeIo {
                audio_ins: 1,
                audio_outs: 0,
                midi_ins: 0,
                midi_outs: 0,
            }),
            _ if id == ids[2] => None,
            _ => Some(NodeIo {
                audio_ins: 2,
                audio_outs: 2,
                midi_ins: 1,
                midi_outs: 1,
            }),
        };
        let links = route(&r.layout(), io);
        let s = |i: usize| Node::Slot(ids[i]);
        let midi = |from, to| Link {
            kind: LinkKind::Midi,
            from,
            sel: PortSel::All,
            to,
        };
        assert_eq!(
            links,
            [
                midi(Node::MidiInput, s(0)),
                audio(s(0), s(1)),
                audio(s(0), s(3)),
                midi(Node::MidiInput, s(3)),
                audio(s(3), Node::Track(t)),
                audio(Node::Track(t), Node::Output),
                midi(s(3), Node::MidiOutput),
            ]
        );
    }

    #[test]
    fn latency_takes_longest_path() {
        let (mut r, _, ids) = rig(&["a", "b", "c"]);
        r.split(ids[1]);
        let t2 = r.add_track(None);
        let s = r.new_slot(desc("x"));
        let x = s.id;
        r.push(t2, s);
        let lat = |id: SlotId| match id {
            _ if id == ids[0] => 10,
            _ if id == ids[1] => 64,
            _ if id == x => 50,
            _ => 1,
        };
        // Track 1: 10 + max(64, 0) + 1 = 75; track 2: 50.
        assert_eq!(path_latency(&r.layout(), lat), 75);
    }
}
