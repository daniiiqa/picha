//! Prints a sample phone card in the current look: `cargo run --example card`
use picha::look::{Look, Tone};

fn main() {
    let look = Look::load();
    let rows = vec![
        vec![(Tone::Primary, "󰄜  Daniel's S21".to_string())],
        vec![(Tone::Muted, "SM-G991U · Android 15 · One UI 7.0".to_string())],
        vec![],
        vec![(Tone::Fg, "󰁹  82%".to_string()), (Tone::Accent, "  charging".to_string()), (Tone::Muted, "  31°C".to_string())],
        vec![(Tone::Fg, "󰖩  Wi-Fi 192.168.1.158:33123".to_string()), (Tone::Muted, "  · encrypted".to_string())],
        vec![(Tone::Fg, "󰋊  41G free".to_string())],
        vec![],
        vec![(Tone::Muted, "drag files onto the window → the phone's Download".to_string())],
        vec![(Tone::Muted, "the clipboard goes both ways".to_string())],
    ];
    print!("{}", look.card("picha · screen", &rows));
    println!(" {}", look.paint(Tone::Accent, "opening the screen…"));
    std::thread::sleep(std::time::Duration::from_secs(6));
}
