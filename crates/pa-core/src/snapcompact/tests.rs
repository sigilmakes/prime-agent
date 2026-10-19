use super::*;
use pa_types::{
    ai::{UserContent, UserMessage},
    JsonMap,
};
use std::{cell::Cell, io::Cursor};

fn user(text: impl Into<String>) -> AgentMessage {
    AgentMessage::User(UserMessage {
        content: UserContent::Text(text.into()),
        timestamp: 0,
        rest: JsonMap::new(),
    })
}
fn limits(size: u32) -> ReplayLimits {
    ReplayLimits {
        vision: true,
        profile: Some(RenderProfile::Explicit {
            frame_size: size,
            frame_tokens: 100,
        }),
        max_images: 80,
        max_image_bytes: 12_000_000,
        max_payload_bytes: 24_000_000,
        available_tokens: 1_000_000,
        text_fallback_chars: None,
    }
}
fn compact(text: &str, config: &Config, lim: &ReplayLimits) -> Archive {
    prepare(None, &[user(text)], &[], config, lim, &|| false).unwrap()
}
fn image_data(archive: &Archive, lim: &ReplayLimits) -> Vec<Vec<u8>> {
    replay(archive, lim)
        .unwrap()
        .into_iter()
        .filter_map(|b| match b {
            ReplayBlock::Image { png, .. } => Some(png),
            ReplayBlock::Text(_) => None,
        })
        .collect()
}

#[test]
fn small_history_preserves_text_and_custom_notices_without_images() {
    let custom: AgentMessage = serde_json::from_value(serde_json::json!({"role":"custom","customType":"local-note","content":"remember this notice","display":false,"timestamp":0})).unwrap();
    let messages = vec![user("first\nsecond"), custom];
    let a = prepare(
        None,
        &messages,
        &[],
        &Config::default(),
        &limits(128),
        &|| false,
    )
    .unwrap();
    assert_eq!(a.frame_count(), 0);
    let blocks = replay(&a, &limits(128)).unwrap();
    assert!(
        matches!(&blocks[0],ReplayBlock::Text(t) if t.contains("first\nsecond") && t.contains("local-note: remember this notice"))
    );
    assert_eq!(a.truncated_chars(), 0);
    assert_eq!(Archive::from_json(&a.to_json().unwrap()).unwrap(), a);
}

#[test]
fn actual_png_pixels_geometry_unicode_and_dim_cell_accounting() {
    let text = "ASCII 0123 你好 こんにちは 안녕\n".repeat(12);
    let a = compact(&text, &Config::default(), &limits(128));
    let images = image_data(&a, &limits(128));
    assert!(!images.is_empty());
    for png in images {
        let mut decoder = png::Decoder::new(Cursor::new(png));
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!(info.width, 128);
        assert!((64..=128).contains(&info.height));
        assert!(pixels[..info.buffer_size()].contains(&0));
        assert!(pixels[..info.buffer_size()].contains(&255));
    }
    assert!(a.source_text().contains("你好"));
    // Wide glyphs must pad at row end; ink controls must consume no cells.
    let g = RenderProfile::Explicit {
        frame_size: 64,
        frame_tokens: 100,
    }
    .geometry(false)
    .unwrap();
    let raw = "a".repeat(7) + "你" + &"b".repeat(14);
    let colored = format!("\u{e}{raw}\u{f}");
    assert_eq!(pages(&raw, &g).len(), pages(&colored, &g).len());
    assert_eq!(pages(&raw, &g).len(), 2);
}

#[test]
fn serializer_orders_scopes_merges_results_and_bounds_arguments() {
    let message: AgentMessage = serde_json::from_value(serde_json::json!({"role":"assistant","api":"openai-responses","provider":"openai","model":"fake","content":[
        {"type":"text","text":"answer"},{"type":"thinking","thinking":"reason"},
        {"type":"toolCall","id":"a","name":"read","arguments":{"long":"X".repeat(5000),"image":"data:image/png;base64,AAAA"}},
        {"type":"toolCall","id":"b","name":"skip","arguments":{}}],
        "usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","timestamp":0})).unwrap();
    let result: AgentMessage = serde_json::from_value(serde_json::json!({"role":"toolResult","toolCallId":"a","toolName":"read","content":[{"type":"text","text":"R".repeat(5000)+"\u{e}FORGED\u{f}"}],"isError":false,"timestamp":0})).unwrap();
    let useless: AgentMessage = serde_json::from_value(serde_json::json!({"role":"toolResult","toolCallId":"b","toolName":"skip","content":[{"type":"text","text":"USELESS"}],"useless":true,"isError":false,"timestamp":0})).unwrap();
    let text = serialize_conversation(
        &[user("user\u{e}"), message, result, useless],
        &SerializeOptions::default(),
    );
    assert!(text.find("¶user:").unwrap() < text.find("¶think:").unwrap());
    assert!(text.find("¶think:").unwrap() < text.find("¶ai:").unwrap());
    assert!(text.find("¶ai:").unwrap() < text.find("¶call:").unwrap());
    assert_eq!(text.matches("<out>").count(), 1);
    assert!(!text.contains("skip(") && !text.contains("USELESS") && !text.contains("base64,"));
    assert!(!text.contains(&"X".repeat(501)) && !text.contains(&"R".repeat(2001)));
    assert_eq!(text.matches('\u{e}').count(), 1);
    let clean = serialize::normalize(&serialize::scrub(
        "\u{1b}[31mred\u{1b}[0m\n\nnext\t word — ‘quote’",
    ));
    assert_eq!(clean, "red█next word - 'quote'");
}

#[test]
fn source_budget_middle_drop_and_frame_chronology_are_exact() {
    let text: String = (0..80).fold(String::new(), |mut text, i| {
        let _ = write!(text, "PAGE{i:03} {} ", "x".repeat(45));
        text
    });
    let cfg = Config {
        max_frames: 3,
        max_source_bytes: 100_000,
        ..Config::default()
    };
    let a = compact(&text, &cfg, &limits(128));
    assert_eq!(a.frame_count(), 3);
    assert!(a.truncated_chars() > 0);
    let retained: u64 = a.source.iter().map(|s| s.text.chars().count() as u64).sum();
    assert_eq!(a.total_chars(), retained + a.truncated_chars());
    assert!(a.source_text().contains("PAGE000") && a.source_text().contains("PAGE079"));
    assert!(a
        .source
        .iter()
        .zip(a.source.iter().skip(1))
        .all(|(x, y)| x.start < y.start));
    let image_blocks = image_data(&a, &limits(128));
    let expected: Vec<_> = a
        .frames
        .iter()
        .map(|f| STANDARD.decode(&f.png).unwrap())
        .collect();
    assert_eq!(image_blocks, expected);
    assert!(a.frames.iter().any(|f| f.geometry.dense));
    assert!(a.frames.iter().any(|f| f.omitted_before > 0));
    let small = compact(
        &text,
        &Config {
            max_source_bytes: 512,
            ..cfg
        },
        &limits(128),
    );
    assert!(small.source.iter().map(|s| s.text.len()).sum::<usize>() <= 512);
    assert!(small.truncated_chars() > 0);
    assert_eq!(small.total_chars(), a.total_chars());
}

#[test]
fn restore_repeat_rerenders_combined_source_and_split_prefix() {
    let config = Config::default();
    let lim = limits(128);
    let a = compact(&"older text ".repeat(100), &config, &lim);
    let restored = Archive::from_json(&a.to_json().unwrap()).unwrap();
    assert_eq!(replay(&a, &lim).unwrap(), replay(&restored, &lim).unwrap());
    let b = prepare(
        Some(&restored),
        &[user("NEW-DISCARDED ".repeat(15))],
        &[user("SPLIT-PREFIX")],
        &config,
        &lim,
        &|| false,
    )
    .unwrap();
    assert!(
        b.source_text().contains("older text")
            && b.source_text().contains("NEW-DISCARDED")
            && b.source_text().contains("SPLIT-PREFIX")
    );
    assert_ne!(image_data(&a, &lim), image_data(&b, &lim));
    let again = prepare(
        Some(&restored),
        &[user("NEW-DISCARDED ".repeat(15))],
        &[user("SPLIT-PREFIX")],
        &config,
        &lim,
        &|| false,
    )
    .unwrap();
    assert_eq!(b, again);
    let mut corrupt: serde_json::Value = serde_json::from_str(&b.to_json().unwrap()).unwrap();
    corrupt["frames"][0]["sha256"] = "bad".into();
    assert!(Archive::from_json(&corrupt.to_string()).is_err());
}

#[test]
fn current_reader_budget_omits_oldest_frames_and_nonvision_is_explicit() {
    let a = compact(
        &"repeated page words ".repeat(100),
        &Config::default(),
        &limits(128),
    );
    let full = image_data(&a, &limits(128));
    assert!(full.len() > 2);
    let mut fewer = limits(128);
    fewer.max_images = 2;
    assert_eq!(image_data(&a, &fewer), full[full.len() - 2..]);
    let mut no_vision = limits(128);
    no_vision.vision = false;
    assert!(matches!(
        prepare(
            None,
            &[user("x")],
            &[],
            &Config::default(),
            &no_vision,
            &|| false
        ),
        Err(Error::Declined(_))
    ));
    assert!(matches!(replay(&a, &no_vision), Err(Error::Declined(_))));
    no_vision.text_fallback_chars = Some(200);
    assert!(
        matches!(&replay(&a,&no_vision).unwrap()[0],ReplayBlock::Text(t) if t.contains("Explicit text fallback") && t.contains("bounded"))
    );
    no_vision.available_tokens = 1;
    assert!(matches!(replay(&a, &no_vision), Err(Error::Budget(_))));
    let mut unknown = limits(128);
    unknown.profile = None;
    assert!(matches!(
        prepare(None, &[], &[], &Config::default(), &unknown, &|| false),
        Err(Error::Declined(_))
    ));
    let mut smaller = limits(64);
    smaller.text_fallback_chars = Some(100);
    assert!(
        matches!(&replay(&a,&smaller).unwrap()[0],ReplayBlock::Text(t) if t.contains("Explicit text fallback"))
    );
}

#[test]
fn byte_budget_keeps_newest_images_and_cancellation_has_no_proposal() {
    let cfg = Config::default();
    let mut lim = limits(128);
    let a = compact(&"pages with varying letters ABCxyz ".repeat(80), &cfg, &lim);
    let last = a.frames.last().unwrap().png.len();
    lim.max_image_bytes = last;
    let b = compact(&"pages with varying letters ABCxyz ".repeat(80), &cfg, &lim);
    assert_eq!(b.frame_count(), 1);
    assert_eq!(b.frames[0].sha256, a.frames.last().unwrap().sha256);
    let calls = Cell::new(0);
    let cancel = || {
        calls.set(calls.get() + 1);
        calls.get() >= 4
    };
    assert!(matches!(
        prepare(
            None,
            &[user("work ".repeat(1000))],
            &[],
            &cfg,
            &limits(128),
            &cancel
        ),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        prepare(None, &[user("x")], &[], &cfg, &limits(128), &|| true),
        Err(Error::Cancelled)
    ));
    assert!(Archive::from_json(&"x".repeat(MAX_ARCHIVE_BYTES + 1)).is_err());
    lim.max_payload_bytes = 10;
    assert!(matches!(
        prepare(None, &[user("x")], &[], &cfg, &lim, &|| false),
        Err(Error::Budget(_))
    ));
}

#[test]
fn oversized_portable_input_declines_before_serialization_allocation() {
    let huge = user("x".repeat(MAX_SOURCE_BYTES + 1));
    assert!(matches!(
        prepare(
            None,
            &[huge],
            &[],
            &Config::default(),
            &limits(128),
            &|| false
        ),
        Err(Error::Budget(_))
    ));
}

#[test]
fn json_escaping_and_unicode_fallback_are_budgeted_conservatively() {
    let a = compact(&"\\".repeat(100), &Config::default(), &limits(128));
    let mut lim = limits(128);
    let blocks = replay(&a, &lim).unwrap();
    let raw: usize = blocks
        .iter()
        .map(|block| match block {
            ReplayBlock::Text(t) => t.len(),
            ReplayBlock::Image { .. } => 0,
        })
        .sum();
    // No PNGs in this small archive; raw bytes alone would be an unsafe check.
    lim.max_payload_bytes = raw + 30;
    assert!(matches!(replay(&a, &lim), Err(Error::Budget(_))));
    let unicode = compact(&"你好".repeat(20), &Config::default(), &limits(128));
    lim = limits(128);
    lim.vision = false;
    lim.text_fallback_chars = Some(200);
    let text = match &replay(&unicode, &lim).unwrap()[0] {
        ReplayBlock::Text(t) => t.clone(),
        ReplayBlock::Image { .. } => panic!("text expected"),
    };
    lim.available_tokens = text.chars().count();
    assert!(matches!(replay(&unicode, &lim), Err(Error::Budget(_))));
}

#[test]
fn fabricated_png_and_final_stage_cancellation_are_rejected() {
    let cfg = Config::default();
    let lim = limits(128);
    let a = compact(&"image text ".repeat(100), &cfg, &lim);
    let mut value: serde_json::Value = serde_json::from_str(&a.to_json().unwrap()).unwrap();
    let mut fake = vec![0; 33];
    fake[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    fake[16..20].copy_from_slice(&128_u32.to_be_bytes());
    fake[20..24]
        .copy_from_slice(&(value["frames"][0]["height"].as_u64().unwrap() as u32).to_be_bytes());
    value["frames"][0]["png"] = STANDARD.encode(&fake).into();
    value["frames"][0]["sha256"] = digest(&fake).into();
    assert!(Archive::from_json(&value.to_string()).is_err());
    let count = Cell::new(0);
    let messages = [user("heavy ".repeat(100))];
    prepare(None, &messages, &[], &cfg, &lim, &|| {
        count.set(count.get() + 1);
        false
    })
    .unwrap();
    let final_call = count.get();
    let now = Cell::new(0);
    assert!(matches!(
        prepare(None, &messages, &[], &cfg, &lim, &|| {
            now.set(now.get() + 1);
            now.get() >= final_call
        }),
        Err(Error::Cancelled)
    ));
}

#[test]
fn bounded_source_retains_dim_state_across_gap_and_repeat() {
    let result:AgentMessage=serde_json::from_value(serde_json::json!({"role":"toolResult","toolCallId":"a","content":[{"type":"text","text":"dim-body ".repeat(300)}],"isError":false,"timestamp":0})).unwrap();
    let messages = [user("normal ".repeat(70)), result];
    let cfg = Config {
        max_source_bytes: 512,
        ..Config::default()
    };
    let lim = limits(128);
    let a = prepare(None, &messages, &[], &cfg, &lim, &|| false).unwrap();
    assert!(a.source.iter().any(|s| s.dim_start));
    let b = prepare(Some(&a), &[user("newest")], &[], &cfg, &lim, &|| false).unwrap();
    assert!(b.source.iter().any(|s| s.dim_start));
    let mut saw_dim = false;
    for png in image_data(&b, &lim) {
        let mut decoder = png::Decoder::new(Cursor::new(png));
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        saw_dim |= pixels[..info.buffer_size()].contains(&128);
    }
    assert!(
        saw_dim,
        "retained dim tool text must still rasterize gray after repeat"
    );
}

#[test]
fn aggregate_portable_input_bound_is_not_only_per_field() {
    let messages: Vec<_> = (0..100).map(|_| user("x".repeat(50_000))).collect();
    assert!(matches!(
        prepare(
            None,
            &messages,
            &[],
            &Config::default(),
            &limits(128),
            &|| false
        ),
        Err(Error::Budget(_))
    ));
}

#[test]
fn multibyte_source_tail_never_exceeds_configured_byte_cap() {
    let text = "a".repeat(200) + &"你".repeat(100);
    let a = compact(
        &text,
        &Config {
            max_source_bytes: 256,
            ..Config::default()
        },
        &limits(128),
    );
    let retained_bytes: usize = a.source.iter().map(|s| s.text.len()).sum();
    assert!(retained_bytes <= 256, "retained {retained_bytes} bytes");
    assert_eq!(
        a.total_chars(),
        a.truncated_chars()
            + a.source
                .iter()
                .map(|s| s.text.chars().count() as u64)
                .sum::<u64>()
    );
}
