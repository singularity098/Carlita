//! Runs the real Carla engine with its "Dummy" driver (no audio hardware)
//! and Carla's internal plugins, exercising the live-chain operations.

use carlita_core::{PluginCategory, PluginDescriptor, PluginFormat, SlotId};
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

#[test]
fn live_chain_on_dummy_driver() {
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
    if driver == "JACK" {
        // Auto-connect hooks the chain up to system:capture/playback.
        let Event::Clients(clients) =
            wait_for(&engine, "clients", |e| matches!(e, Event::Clients(_)))
        else {
            unreachable!()
        };
        assert!(clients.iter().any(|c| c.name == "system"), "{clients:?}");
    }

    // Build a chain of three plugins while the engine runs.
    for (i, label) in ["audiogain_s", "lfo", "audiogain_s"].iter().enumerate() {
        engine.send(Command::AddPlugin {
            load: load(i as u64, label),
            index: None,
        });
        wait_for(
            &engine,
            "plugin loaded",
            |e| matches!(e, Event::PluginLoaded { slot, .. } if slot.0 == i as u64),
        );
    }

    // Loading a plugin that does not exist leaves a pass-through placeholder.
    engine.send(Command::AddPlugin {
        load: load(9, "definitely-not-a-plugin"),
        index: Some(0),
    });
    wait_for(
        &engine,
        "plugin failed",
        |e| matches!(e, Event::PluginFailed { slot, .. } if slot.0 == 9),
    );

    // Reorder and bypass while running.
    engine.send(Command::MovePlugin { from: 0, to: 3 });
    engine.send(Command::SetBypass {
        slot: SlotId(1),
        bypassed: true,
    });
    wait_for(
        &engine,
        "bypass",
        |e| matches!(e, Event::BypassChanged { slot, bypassed: true } if slot.0 == 1),
    );

    // Meters report one latency entry per slot, in the new order.
    let Event::Meters(m) = wait_for(
        &engine,
        "meters",
        |e| matches!(e, Event::Meters(m) if m.latencies.len() == 4),
    ) else {
        unreachable!()
    };
    let order: Vec<u64> = m.latencies.iter().map(|(s, _)| s.0).collect();
    assert_eq!(order, vec![0, 1, 2, 9]);
    assert!(m.total_latency_ms() > 0.0);

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
    assert_eq!(states.len(), 4);
    let gain_state = states
        .iter()
        .find(|(s, _)| s.0 == 0)
        .and_then(|(_, st)| st.clone())
        .expect("state saved");
    assert!(
        gain_state.contains("<Info>") || gain_state.contains("<Data>"),
        "unexpected state: {gain_state}"
    );

    // Reload a whole chain with restored state.
    engine.send(Command::LoadChain {
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
    wait_for(&engine, "chain loaded", |e| matches!(e, Event::ChainLoaded));
    let Event::Meters(m) = wait_for(
        &engine,
        "meters",
        |e| matches!(e, Event::Meters(m) if m.latencies.len() == 2),
    ) else {
        unreachable!()
    };
    assert_eq!(m.latencies[0].0, SlotId(20));

    engine.send(Command::RemovePlugin { slot: SlotId(20) });
    wait_for(
        &engine,
        "removed",
        |e| matches!(e, Event::PluginRemoved { slot, state: Some(_) } if slot.0 == 20),
    );

    engine.send(Command::Stop);
    wait_for(&engine, "stopped", |e| matches!(e, Event::Stopped));
}
