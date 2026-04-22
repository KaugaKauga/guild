//! ASCII art banner for the Familiar daemon startup screen.

use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use crossterm::ExecutableCommand;
use std::io::stdout;

const BANNER: &str = r#"
                      /\       /\
                     /  \_/\__/  \
                    / \  _/\_  / \
                   /   \/    \/   \
                  / /|          |\ \
                 / / | FAMILIAR | \ \
                / /  |  HOUSE   |  \ \
               /_/  /||  ||  ||\   \_\
              |  | / ||  ||  || \ |  |
              |🔮| | |+--++--+| | |📖|
              |  | | ||/\||/\|| | |  |
              |  |_| ||''||''|| |_|  |
              |  |   |+--++--+|   |  |
              | .|   ||  ()  ||   |. |
              |/ |   ||      ||   | \|
             /|  |   |+------+|   |  |\
            /_|__|___|________|___|__|_\
           |  ~~  {come in, we're open}  |
           |_____________________________|
"#;

const TAGLINE: &str = r#"
    ┌─────────────────────────────────────────────────────────────────┐
    │  "From issue to pull request, your familiar works through the  │
    │   night so you don't have to."                                 │
    └─────────────────────────────────────────────────────────────────┘
"#;

/// Print the familiar banner with colors to stdout.
pub fn print_banner() {
    let mut out = stdout();

    let _ = out.execute(SetForegroundColor(Color::DarkYellow));
    let _ = out.execute(Print(BANNER));
    let _ = out.execute(SetForegroundColor(Color::Cyan));
    let _ = out.execute(Print(TAGLINE));
    let _ = out.execute(ResetColor);
    let _ = out.execute(Print("\n"));
}
