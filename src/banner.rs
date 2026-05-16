pub const BANNER: &str = r"
██╗     ██╗      █████╗ ███╗   ███╗ █████╗ ██████╗  █████╗ ███████╗██╗  ██╗
██║     ██║     ██╔══██╗████╗ ████║██╔══██╗██╔══██╗██╔══██╗██╔════╝██║  ██║
██║     ██║     ███████║██╔████╔██║███████║██║  ██║███████║███████╗███████║
██║     ██║     ██╔══██║██║╚██╔╝██║██╔══██║██║  ██║██╔══██║╚════██║██╔══██║
███████╗███████╗██║  ██║██║ ╚═╝ ██║██║  ██║██████╔╝██║  ██║███████║██║  ██║
╚══════╝╚══════╝╚═╝  ╚═╝╚═╝     ╚═╝╚═╝  ╚═╝╚═════╝ ╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝
";

/// Compact LlamaDash glyph for the TUI's Logo panel.
///
/// Doubled-stroke "L-" — a stylised L with a horizontal mid-stroke
/// suggesting the trailing dash in "LlamaDash". Designed to fit inside
/// a ~22-column inner area at 5 rows tall.
///
/// Lines are intentionally not trimmed; the `logo_pane` renderer drops
/// the leading newline and pads each line as needed.
pub const COMPACT_BANNER: &str = r"
  ╔╗
  ║║
  ║║     ══
  ║║
  ╚╩═══════
";
