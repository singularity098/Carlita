//! Loads plugin settings imported from a REAPER project into real plugins
//! (LSP's LV2 plugins, when installed) on Carla's Dummy driver (or JACK),
//! and checks that the plugins took them.

use carlita_core::rig::{BlockLayout, RigLayout, TrackLayout};
use carlita_core::{
    reaper, PluginCategory, PluginDescriptor, PluginFormat, SlotId, TrackId, TrackInput, TrackMidi,
};
use carlita_engine::*;
use std::time::{Duration, Instant};

fn lv2(name: &str, uri: &str) -> PluginDescriptor {
    PluginDescriptor {
        format: PluginFormat::Lv2,
        arch: Default::default(),
        name: name.into(),
        label: uri.into(),
        maker: String::new(),
        filename: String::new(),
        unique_id: 0,
        category: PluginCategory::Dynamics,
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

/// A short mono WAV file (an impulse).
fn write_wav(path: &std::path::Path) {
    let samples: Vec<i16> = (0..64).map(|i| if i == 0 { i16::MAX } else { 0 }).collect();
    let data_len = (samples.len() * 2) as u32;
    let mut b = Vec::new();
    b.extend(b"RIFF");
    b.extend((36 + data_len).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes()); // PCM
    b.extend(1u16.to_le_bytes()); // mono
    b.extend(48_000u32.to_le_bytes());
    b.extend((48_000u32 * 2).to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(data_len.to_le_bytes());
    for s in samples {
        b.extend(s.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

/// The value saved for an LV2 parameter in a Carla state.
fn param(state: &str, symbol: &str) -> f64 {
    let at = state
        .find(&format!("<Symbol>{symbol}</Symbol>"))
        .unwrap_or_else(|| panic!("no parameter {symbol}"));
    let rest = &state[at..];
    let v = rest
        .split("<Value>")
        .nth(1)
        .unwrap()
        .split('<')
        .next()
        .unwrap();
    v.trim().parse().unwrap()
}

#[test]
fn imported_lv2_settings_reach_the_plugin() {
    let lsp = [
        "/usr/lib/lv2/lsp-plugins.lv2",
        "/usr/local/lib/lv2/lsp-plugins.lv2",
    ];
    if !lsp.iter().any(|p| std::path::Path::new(p).exists()) {
        eprintln!("LSP LV2 plugins not installed; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // Carla links files an LV2 state names into a folder under the working
    // directory.
    std::env::set_current_dir(dir.path()).unwrap();
    let ir = dir.path().join("Best IR's & more.wav");
    write_wav(&ir);

    // As REAPER 7 saves them (from a real project).
    let project = format!(
        r#"<REAPER_PROJECT 0.1 "7.84/linux-x86_64" 1793717047 0
  <TRACK {{8BCD8706}}
    NAME "Pads"
    <FXCHAIN
      BYPASS 0 0 0
      <LV2 "LV2: LSP Compressor Stereo (LSP LV2)" http://lsp-plug.in/plugins/lv2/compressor_stereo ""
        CFG 0 1589 923 ""
        P showpmx 0 in2lk 0 lk2in 0 lk2sc 0 enabled 1 g_in 1 g_out 0.266809999942779541 showmx 0 showsc 0 pause 0
        P clear 0 ssplit 0 sscs 0 sct 0 scm 1 sla 0 scl 0 scs 0 scr 10 scp 1 shpm 0 shpf 10 slpm 0 slpf 20000 cm 2
        P al 0.251190006732940674 at 20 rrl 0 rt 100 hold 0 cr 4 kn 0.501179993152618408 bth 0.000250999990385026
        P bsa 8.880556106567382812 mk 1 cdr 0 cwt 1 cdw 100 slv_l 1 elv_l 1 grv_l 1 ilv_l 1 olv_l 1 slv_r 1 elv_r 1
        P grv_r 1 ilv_r 1 olv_r 1
        <STATE
          S http://lsp-plug.in/plugins/lv2/compressor_stereo/ports#link "" 3
          <L http://lsp-plug.in/plugins/lv2/compressor_stereo/preset_state "" http://lsp-plug.in/types/lv2/PresetState 3
            S http://lsp-plug.in/types/lv2/PresetState#name Smush 3
            I http://lsp-plug.in/types/lv2/PresetState#flags 1 3
            I http://lsp-plug.in/types/lv2/PresetState#tab 0 3
          >
          <T http://lsp-plug.in/plugins/lv2/compressor_stereo/KVT 3
          >
        >
      >
      BYPASS 0 0 0
      <LV2 "LV2: LSP Impulse Responses Mono (LSP LV2)" http://lsp-plug.in/plugins/lv2/impulse_responses_mono ""
        P enabled 1 fft 6 dry 0 wet 0.707949995994567871 drywet 100 g_out 1 psh 0 ihc 0 itc 0 ifi 0 ifo 0 ils 0
        <STATE
          P http://lsp-plug.in/plugins/lv2/impulse_responses_mono/ports#ifn "{}" 3
        >
      >
    >
  >
>
"#,
        ir.display()
    );
    let catalog = [
        lv2(
            "LSP Compressor Stereo",
            "http://lsp-plug.in/plugins/lv2/compressor_stereo",
        ),
        lv2(
            "LSP Impulse Responses Mono",
            "http://lsp-plug.in/plugins/lv2/impulse_responses_mono",
        ),
    ];
    let imp = reaper::import_str(&project, "Pads", &catalog).unwrap();
    assert!(imp.report.problems().is_empty(), "{:?}", imp.report);
    let slots: Vec<_> = imp.preset.rig.slots().cloned().collect();
    assert_eq!(slots.len(), 2);

    let engine = EngineHandle::spawn();
    engine.send(Command::QueryDrivers);
    let Event::Drivers(drivers) = wait_for(&engine, "drivers", |e| matches!(e, Event::Drivers(_)))
    else {
        unreachable!()
    };
    // Carla's Dummy driver, or JACK (CI runs `jackd -d dummy`).
    let driver = if drivers.iter().any(|d| d.name == "Dummy") {
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
        Event::Started { .. } => {}
        Event::StartFailed(e)
            if driver == "JACK" && std::env::var_os("CARLITA_REQUIRE_ENGINE").is_none() =>
        {
            eprintln!("no JACK server running ({e}); skipping");
            return;
        }
        other => panic!("engine did not start: {other:?}"),
    }
    engine.send(Command::SetLayout(RigLayout {
        tracks: vec![TrackLayout {
            id: TrackId(100),
            input: TrackInput::stereo(),
            midi: TrackMidi::All,
            volume: 1.0,
            muted: false,
            blocks: slots.iter().map(|s| BlockLayout::Slot(s.id)).collect(),
        }],
    }));
    for s in &slots {
        engine.send(Command::AddPlugin {
            load: PluginLoad {
                slot: s.id,
                plugin: s.plugin.clone(),
                name: s.name.clone(),
                bypassed: false,
                state: s.state.clone(),
            },
        });
        match wait_for(
            &engine,
            "plugin loaded",
            |e| matches!(e, Event::PluginLoaded { slot, .. } | Event::PluginFailed { slot, .. } if *slot == s.id),
        ) {
            Event::PluginLoaded { .. } => {}
            other => panic!("{} did not load: {other:?}", s.name),
        }
    }
    // Let the plugins settle (LSP loads the impulse file in the background).
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let _ = engine.events().recv_timeout(Duration::from_millis(100));
    }

    let (tx, rx) = crossbeam_channel::bounded(1);
    engine.send(Command::CaptureStates { reply: tx });
    let states = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let state = |id: SlotId| {
        states
            .iter()
            .find(|(s, _)| *s == id)
            .and_then(|(_, st)| st.clone())
            .expect("state captured")
    };

    let comp = state(slots[0].id);
    assert!((param(&comp, "g_out") - 0.26681).abs() < 1e-4, "{comp}");
    assert_eq!(param(&comp, "cr"), 4.0);
    assert_eq!(param(&comp, "at"), 20.0);
    assert_eq!(param(&comp, "cm"), 2.0);
    assert!(comp.contains("<Options>"), "{comp}");

    let irs = state(slots[1].id);
    assert!((param(&irs, "wet") - 0.70795).abs() < 1e-4, "{irs}");
    assert_eq!(param(&irs, "fft"), 6.0);
    assert!(irs.contains("Best IR&apos;s &amp; more.wav"), "{irs}");

    engine.send(Command::Stop);
}
