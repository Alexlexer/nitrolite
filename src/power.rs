//! Reproduce NitroSense's AC/DC mode switch.
//!
//! The laptop's profile (`"ACDCModeSet": true`) expects the operating mode to
//! follow the power source: ECO on battery, the user's chosen mode on AC. That
//! logic lived in NitroSense, not in the Acer service stack — verified by
//! unplugging with NitroSense uninstalled: the mode stayed Balanced and
//! `AASSvc` was running throughout.
//!
//! Without this, unplugging while in Performance silently keeps the performance
//! profile on battery.

/// ECO, the power-saving mode the vendor applies on battery.
pub const ECO: u8 = 6;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Event {
    Plugged,
    Unplugged,
    None,
}

/// Compare the previous power source with the current one.
pub fn classify(previous: Option<bool>, current_on_ac: bool) -> Event {
    match previous {
        // First observation is not a transition: adopting a mode we did not
        // choose would be surprising at startup.
        None => Event::None,
        Some(prev) if prev == current_on_ac => Event::None,
        Some(_) if current_on_ac => Event::Plugged,
        Some(_) => Event::Unplugged,
    }
}

/// Mode to apply for an event, or `None` to leave the current mode alone.
pub fn mode_for(event: Event, preferred: u8) -> Option<u8> {
    match event {
        Event::Unplugged => Some(ECO),
        Event::Plugged => Some(preferred),
        Event::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sighting_is_not_a_transition() {
        assert_eq!(classify(None, true), Event::None);
        assert_eq!(classify(None, false), Event::None);
        assert_eq!(mode_for(Event::None, 4), None);
    }

    #[test]
    fn unplugging_enters_eco_and_plugging_returns_the_preferred_mode() {
        assert_eq!(classify(Some(true), false), Event::Unplugged);
        assert_eq!(mode_for(Event::Unplugged, 4), Some(ECO));

        assert_eq!(classify(Some(false), true), Event::Plugged);
        assert_eq!(mode_for(Event::Plugged, 4), Some(4));
        assert_eq!(mode_for(Event::Plugged, 0), Some(0));
        assert_eq!(mode_for(Event::Plugged, 1), Some(1));
    }

    #[test]
    fn steady_state_never_writes() {
        assert_eq!(classify(Some(true), true), Event::None);
        assert_eq!(classify(Some(false), false), Event::None);
    }

    #[test]
    fn eco_is_never_the_restored_mode() {
        // Restoring ECO on AC would defeat the point of remembering a choice.
        for preferred in [0u8, 1, 4] {
            assert_ne!(mode_for(Event::Plugged, preferred), Some(ECO));
        }
    }
}
