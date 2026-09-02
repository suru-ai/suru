//! Terminal capabilities and colors observed before the Application starts.

/// One RGB color reported by the terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl TerminalColor {
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

/// The colors a startup terminal probe reported.
///
/// Every value is optional because terminals may answer only part of the OSC
/// query. An absent probe is distinct from a partial one and is carried by
/// [`TerminalFacts::probe`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalColorProbe {
    pub palette: [Option<TerminalColor>; 16],
    pub foreground: Option<TerminalColor>,
    pub background: Option<TerminalColor>,
}

impl TerminalColorProbe {
    pub const fn new(
        palette: [Option<TerminalColor>; 16],
        foreground: Option<TerminalColor>,
        background: Option<TerminalColor>,
    ) -> Self {
        Self {
            palette,
            foreground,
            background,
        }
    }
}

/// Everything the terminal told Suru that affects Theme resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalFacts {
    pub probe: Option<TerminalColorProbe>,
    pub truecolor: bool,
}

impl TerminalFacts {
    pub const fn new(probe: Option<TerminalColorProbe>, truecolor: bool) -> Self {
        Self { probe, truecolor }
    }

    pub const fn unprobed(truecolor: bool) -> Self {
        Self::new(None, truecolor)
    }
}

impl Default for TerminalFacts {
    fn default() -> Self {
        Self::unprobed(false)
    }
}
