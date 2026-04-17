//! ASCII art banner for the Guild daemon startup screen.

use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use crossterm::ExecutableCommand;
use std::io::stdout;

const BANNER: &str = r#"
                          ⚒️   ⚒️
                     ╔══════════════════╗
                     ║  ┌──────────────┐║
                     ║  │  ♜  GUILD  ♜ │║
                     ║  └──────────────┘║
                     ╚══════╦════╦══════╝
                       ┌────╨────╨────┐
                       │  ╔══════════╗│
                       │  ║ AUTONOMOUS║│
                       │  ║ SOFTWARE  ║│
                       │  ║ FACTORY   ║│
                       │  ╚══════════╝│
                  ┌────┤              ├────┐
                  │⚙️  │   ┌──────┐   │  ⚙️│
                  │    │   │ ◆◆◆◆ │   │    │
                  │    │   │ ◆◆◆◆ │   │    │
                  │    │   └──────┘   │    │
                  └────┤     ║║║║     ├────┘
                       │   ╔╧╧╧╧╗    │
                       │   ║FIRE║    │
                       │   ╚════╝    │
                  ═════╧═════════════╧═════
                  ░░▒▒▓▓ THE FORGE ▓▓▒▒░░
                  ═════════════════════════
"#;

const TAGLINE: &str = r#"
    ┌─────────────────────────────────────────────────┐
    │  "From issue to pull request, the guild works   │
    │   through the night so you don't have to."      │
    └─────────────────────────────────────────────────┘
"#;

/// Print the guild banner with colors to stdout.
pub fn print_banner() {
    let mut out = stdout();

    let _ = out.execute(SetForegroundColor(Color::DarkYellow));
    let _ = out.execute(Print(BANNER));
    let _ = out.execute(SetForegroundColor(Color::Cyan));
    let _ = out.execute(Print(TAGLINE));
    let _ = out.execute(ResetColor);
    let _ = out.execute(Print("\n"));
}
