//! Runs the real Carla engine with its "Dummy" driver (no audio hardware)
//! and Carla's internal plugins, exercising tracks, parallel blocks and the
//! live-chain operations.

use carlita_core::rig::{BlockLayout, RigLayout, TrackLayout};
use carlita_core::{
    PluginCategory, PluginDescriptor, PluginFormat, SlotId, TrackId, TrackInput, TrackMidi,
};
use carlita_engine::*;
use std::time::{Duration, Instant};

fn internal(label: &str) -> PluginDescriptor {
    PluginDescriptor {
        format: PluginFormat::Internal,
        arch: Default::default(),
        name: label.into(),
        label: label.into(),
        maker: String::new(),
        filename: String::new(),
        unique_id: 0,
        category: PluginCategory::Utility,
        audio_ins: 2,
        audio_outs: 2,
        midi_ins: 0,
        midi_outs: 0,
        parameters: 1,
        is_synth: false,
        has_custom_ui: false,
        yabridge: false,
    }
}

fn load(slot: u64, label: &str) -> PluginLoad {
    PluginLoad {
        slot: SlotId(slot),
        plugin: internal(label),
        name: format!("{label}-{slot}"),
        bypassed: false,
        state: None,
    }
}

fn wait_for(engine: &EngineHandle, what: &str, mut pred: impl FnMut(&Event) -> bool) -> Event {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(e) = engine.events().recv_timeout(Duration::from_millis(100)) {
            if let Event::Error(msg) = &e {
                eprintln!("engine error: {msg}");
            }
            if pred(&e) {
                return e;
            }
        }
    }
    panic!("timed out waiting for {what}");
}

fn track(id: u64, input: TrackInput, blocks: Vec<BlockLayout>) -> TrackLayout {
    TrackLayout {
        id: TrackId(id),
        input,
        midi: TrackMidi::All,
        volume: 1.0,
        muted: false,
        blocks,
    }
}

fn slot(id: u64) -> BlockLayout {
    BlockLayout::Slot(SlotId(id))
}

/// Waits until the rig's graph (Carla's internal one, or under JACK the
/// JACK graph) satisfies `pred`.
fn wait_graph(engine: &EngineHandle, what: &str, pred: impl Fn(&[(String, String)]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = Vec::new();
    while Instant::now() < deadline {
        let (tx, rx) = crossbeam_channel::bounded(1);
        engine.send(Command::DescribeGraph { reply: tx });
        last = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if pred(&last) {
            return;
        }
        while engine.events().try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}; graph: {last:#?}");
}

fn has(edges: &[(String, String)], from: &str, to: &str) -> bool {
    edges.iter().any(|(a, b)| a == from && b == to)
}

#[test]
fn live_rig_on_dummy_driver() {
    let engine = EngineHandle::spawn();

    engine.send(Command::QueryDrivers);
    let Event::Drivers(drivers) = wait_for(&engine, "drivers", |e| matches!(e, Event::Drivers(_)))
    else {
        unreachable!()
    };
    let names: Vec<_> = drivers.iter().map(|d| d.name.as_str()).collect();
    eprintln!("drivers: {names:?}");
    // Prefer Carla's Dummy driver; otherwise use JACK (CI runs
    // `jackd -d dummy`). Skip when neither is usable.
    let driver = if names.contains(&"Dummy") {
        "Dummy"
    } else {
        "JACK"
    };

    engine.send(Command::Start(EngineConfig {
        driver: driver.into(),
        isolate_plugins: false,
        sample_rate: 48_000,
        buffer_size: 256,
        ..Default::default()
    }));
    match wait_for(&engine, "start", |e| {
        matches!(e, Event::Started { .. } | Event::StartFailed(_))
    }) {
        Event::Started { sample_rate, .. } => assert!(sample_rate > 0.0),
        Event::StartFailed(e)
            if driver == "JACK" && std::env::var_os("CARLITA_REQUIRE_ENGINE").is_none() =>
        {
            eprintln!("no JACK server running ({e}); skipping");
            return;
        }
        other => panic!("engine did not start: {other:?}"),
    }
    // Under JACK each plugin is a JACK client, and tracks connect straight
    // to the hardware ports.
    let jack = driver == "JACK";
    let (hw_in, hw_out) = if jack {
        ("system", "system")
    } else {
        ("Audio Input", "Audio Output")
    };
    let (mut clients, mut inputs) = (None, None);
    wait_for(&engine, "clients and hardware inputs", |e| {
        match e {
            Event::Clients(c) => clients = Some(c.clone()),
            Event::Hardware { inputs: i, .. } if !i.is_empty() => inputs = Some(i.clone()),
            _ => {}
        }
        inputs.is_some() && (clients.is_some() || !jack)
    });
    let inputs = inputs.unwrap();
    if let Some(clients) = clients {
        assert!(clients.iter().any(|c| c.name == "system"), "{clients:?}");
        assert!(
            !clients.iter().any(|c| c.name.starts_with("Carlita")),
            "{clients:?}"
        );
    }
    assert!(inputs.len() >= 2, "{inputs:?}");

    // Two tracks: "a → [b | dry] → c" and, from input 1 only, "d".
    let layout = RigLayout {
        tracks: vec![
            track(
                100,
                TrackInput::stereo(),
                vec![
                    slot(0),
                    BlockLayout::Parallel(vec![vec![SlotId(1)], vec![]]),
                    slot(2),
                ],
            ),
            track(101, TrackInput::new([1]), vec![slot(3)]),
        ],
    };
    engine.send(Command::SetLayout(layout.clone()));
    for i in 0..4u64 {
        engine.send(Command::AddPlugin {
            load: load(i, "audiogain_s"),
        });
        wait_for(
            &engine,
            "plugin loaded",
            |e| matches!(e, Event::PluginLoaded { slot, .. } if slot.0 == i),
        );
    }
    let n = |i: u64| format!("Carlita.{i} audiogain_s-{i}");
    wait_graph(&engine, "two tracks wired", |g| {
        has(g, hw_in, &n(0))
            && has(g, hw_in, "Carlita.in-100")
            && has(g, hw_in, "Carlita.in-101")
            && has(g, &n(0), &n(1))
            && has(g, &n(1), &n(2))
            // The dry lane.
            && has(g, &n(0), &n(2))
            && has(g, &n(2), "Carlita.track-100")
            && has(g, hw_in, &n(3))
            && has(g, &n(3), "Carlita.track-101")
            && has(g, "Carlita.track-100", "Carlita.out")
            && has(g, "Carlita.track-101", "Carlita.out")
            && has(g, "Carlita.out", hw_out)
            // Never a hardware input straight to the speakers.
            && !has(g, hw_in, hw_out)
    });

    // Loading a plugin that does not exist leaves a pass-through
    // placeholder, which is wired like the plugin would have been.
    let mut layout2 = layout.clone();
    layout2.tracks[1].blocks.insert(0, slot(9));
    engine.send(Command::SetLayout(layout2.clone()));
    engine.send(Command::AddPlugin {
        load: load(9, "definitely-not-a-plugin"),
    });
    wait_for(
        &engine,
        "plugin failed",
        |e| matches!(e, Event::PluginFailed { slot, .. } if slot.0 == 9),
    );
    let ph = "Carlita.9 definitely-not-a-plugin-9";
    wait_graph(&engine, "placeholder wired", |g| {
        has(g, hw_in, ph) && has(g, ph, &n(3)) && !has(g, hw_in, &n(3))
    });

    // Move c to the front of track 1 and mute track 2, while running.
    layout2.tracks[0].blocks = vec![
        slot(2),
        slot(0),
        BlockLayout::Parallel(vec![vec![SlotId(1)], vec![]]),
    ];
    layout2.tracks[1].muted = true;
    engine.send(Command::SetLayout(layout2.clone()));
    wait_graph(&engine, "moved and muted", |g| {
        has(g, hw_in, &n(2))
            && has(g, &n(2), &n(0))
            && has(g, &n(1), "Carlita.track-100")
            && has(g, &n(0), "Carlita.track-100")
            && !has(g, &n(1), &n(2))
            && !has(g, "Carlita.track-101", "Carlita.out")
    });

    // A track with no inputs takes no audio.
    let mut quiet = layout2.clone();
    quiet.tracks[1].input = TrackInput::none();
    // An empty track whose bus is not there yet plays into nothing, not
    // into the hardware outputs.
    quiet.tracks.push(track(102, TrackInput::stereo(), vec![]));
    engine.send(Command::SetLayout(quiet));
    wait_graph(&engine, "input removed", |g| {
        !has(g, hw_in, ph)
            && !has(g, hw_in, "Carlita.in-101")
            && has(g, hw_in, &n(2))
            && has(g, hw_in, "Carlita.track-102")
            && !has(g, hw_in, hw_out)
    });
    engine.send(Command::SetLayout(layout2.clone()));

    // Bypass while running.
    engine.send(Command::SetBypass {
        slot: SlotId(1),
        bypassed: true,
    });
    wait_for(
        &engine,
        "bypass",
        |e| matches!(e, Event::BypassChanged { slot, bypassed: true } if slot.0 == 1),
    );

    // Meters: one latency per plugin and a level for each track.
    let Event::Meters(m) = wait_for(
        &engine,
        "meters",
        |e| matches!(e, Event::Meters(m) if m.latencies.len() == 5),
    ) else {
        unreachable!()
    };
    let mut ids: Vec<u64> = m.latencies.iter().map(|(s, _)| s.0).collect();
    ids.sort();
    assert_eq!(ids, vec![0, 1, 2, 3, 9]);
    assert_eq!(m.tracks.len(), 2);
    assert_eq!(m.track_inputs.len(), 2);
    assert!(m.total_latency_ms() > 0.0);

    // Removing a track drops its bus.
    let mut layout3 = layout2.clone();
    layout3.tracks.truncate(1);
    engine.send(Command::SetLayout(layout3));
    engine.send(Command::RemovePlugin { slot: SlotId(3) });
    engine.send(Command::RemovePlugin { slot: SlotId(9) });
    wait_graph(&engine, "track removed", |g| {
        !g.iter()
            .any(|(a, b)| a.ends_with("-101") || b.ends_with("-101"))
            && has(g, "Carlita.track-100", "Carlita.out")
    });

    // Parameters for the generic editor.
    engine.send(Command::QueryParameters { slot: SlotId(0) });
    let Event::Parameters { params, .. } =
        wait_for(&engine, "params", |e| matches!(e, Event::Parameters { .. }))
    else {
        unreachable!()
    };
    assert!(!params.is_empty());
    engine.send(Command::SetParameter {
        slot: SlotId(0),
        index: 0,
        value: 0.5,
    });

    // State capture for presets.
    let (tx, rx) = crossbeam_channel::bounded(1);
    engine.send(Command::CaptureStates { reply: tx });
    let states = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(states.len(), 3);
    let gain_state = states
        .iter()
        .find(|(s, _)| s.0 == 0)
        .and_then(|(_, st)| st.clone())
        .expect("state saved");
    assert!(
        gain_state.contains("<Info>") || gain_state.contains("<Data>"),
        "unexpected state: {gain_state}"
    );

    // Reload a whole rig with restored state.
    engine.send(Command::LoadRig {
        layout: RigLayout {
            tracks: vec![track(100, TrackInput::stereo(), vec![slot(20), slot(21)])],
        },
        loads: vec![
            PluginLoad {
                state: Some(gain_state),
                ..load(20, "audiogain_s")
            },
            PluginLoad {
                bypassed: true,
                ..load(21, "audiogain_s")
            },
        ],
    });
    wait_for(&engine, "rig loaded", |e| matches!(e, Event::RigLoaded));
    wait_graph(&engine, "rig reloaded", |g| {
        has(g, hw_in, &n(20))
            && has(g, &n(20), &n(21))
            && has(g, &n(21), "Carlita.track-100")
            && !g.iter().any(|(a, _)| a == &n(0))
    });

    engine.send(Command::RemovePlugin { slot: SlotId(20) });
    wait_for(
        &engine,
        "removed",
        |e| matches!(e, Event::PluginRemoved { slot, state: Some(_) } if slot.0 == 20),
    );

    if jack {
        midi_devices_per_track(&engine);
    }

    engine.send(Command::Stop);
    wait_for(&engine, "stopped", |e| matches!(e, Event::Stopped));
}

/// Two MIDI "devices" (JACK clients named like a2jmidid's), each feeding a
/// different track, and a track that takes none.
fn midi_devices_per_track(engine: &EngineHandle) {
    let spawn = |name: &str| {
        std::process::Command::new("jack_midiseq")
            .args([name, "48000", "0", "60", "1000"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()
    };
    let (Some(mut keys), Some(mut pads)) = (spawn("a2j-keys"), spawn("a2j-pads")) else {
        eprintln!("jack_midiseq not found; skipping the MIDI device check");
        return;
    };
    let devices = wait_for(
        engine,
        "MIDI devices",
        |e| matches!(e, Event::Hardware { midi, .. } if midi.len() >= 2),
    );
    let Event::Hardware {
        midi, merged_midi, ..
    } = devices
    else {
        unreachable!()
    };
    assert!(!merged_midi);
    let id = |client: &str| {
        midi.iter()
            .find(|d| d.id.starts_with(client))
            .unwrap_or_else(|| panic!("{client} not in {midi:?}"))
            .id
            .clone()
    };
    let (keys_id, pads_id) = (id("a2j-keys:"), id("a2j-pads:"));

    let thru = |slot: u64| PluginLoad {
        plugin: PluginDescriptor {
            audio_ins: 0,
            audio_outs: 0,
            midi_ins: 1,
            midi_outs: 1,
            ..internal("midithrough")
        },
        ..load(slot, "midithrough")
    };
    let mut t1 = track(200, TrackInput::none(), vec![slot(30)]);
    t1.midi = TrackMidi::Devices(vec![keys_id]);
    let mut t2 = track(201, TrackInput::none(), vec![slot(31)]);
    t2.midi = TrackMidi::Devices(vec![pads_id.clone()]);
    let t3 = track(202, TrackInput::none(), vec![slot(32)]);
    let mut layout = RigLayout {
        tracks: vec![t1, t2, t3],
    };
    engine.send(Command::LoadRig {
        loads: vec![thru(30), thru(31), thru(32)],
        layout: layout.clone(),
    });
    wait_for(engine, "MIDI rig loaded", |e| matches!(e, Event::RigLoaded));
    let n = |i: u64| format!("Carlita.{i} midithrough-{i}");
    wait_graph(engine, "MIDI per track", |g| {
        has(g, "a2j-keys", &n(30))
            && !has(g, "a2j-pads", &n(30))
            && has(g, "a2j-pads", &n(31))
            && !has(g, "a2j-keys", &n(31))
            // A track that takes every device.
            && has(g, "a2j-keys", &n(32))
            && has(g, "a2j-pads", &n(32))
    });

    // Both devices on track 1, none on track 3.
    layout.tracks[0].midi.toggle(&pads_id, &[]);
    layout.tracks[2].midi = TrackMidi::Devices(vec![]);
    engine.send(Command::SetLayout(layout));
    wait_graph(engine, "MIDI changed", |g| {
        has(g, "a2j-keys", &n(30))
            && has(g, "a2j-pads", &n(30))
            && !g.iter().any(|(_, b)| b == &n(32))
    });

    let _ = keys.kill();
    let _ = pads.kill();
    let _ = keys.wait();
    let _ = pads.wait();
}
