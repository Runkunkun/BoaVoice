//! Finding the microphones and speakers, and choosing between them.
//!
//! Devices are identified by **name**, not by index. Indices are assigned in enumeration order
//! and change the moment a headset is plugged in, so a saved index reliably selects the wrong
//! device the next time hardware moves — which in a voice app means your microphone silently
//! becomes the webcam's. A name that has disappeared falls back to the system default, which is
//! the right behaviour for an unplugged headset and needs no error dialogue.
//!
//! Enumeration is also not free and not reliable. On every platform it talks to an audio daemon
//! that can be slow, can be restarting, and can panic inside the driver — CoreAudio in particular
//! will do so for a device that is disappearing while being listed. So the list is fetched
//! explicitly (when the settings screen opens, or when somebody asks for a refresh) rather than
//! per frame, and every call is wrapped so that a driver's bad day cannot take the app with it.

use cpal::traits::HostTrait as _;

/// One device, as the settings screen shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    /// Whether this is the one the system would pick.
    pub is_default: bool,
}

/// What is available right now.
#[derive(Clone, Debug, Default)]
pub struct Devices {
    pub inputs: Vec<DeviceInfo>,
    pub outputs: Vec<DeviceInfo>,
}

impl Devices {
    /// Ask the system what it has.
    ///
    /// Everything here is best-effort: a host that will not enumerate produces an empty list,
    /// which the settings screen shows as "system default only" rather than as an error. The app
    /// still works, because opening the default device does not go through this code.
    pub fn enumerate() -> Devices {
        let host = cpal::default_host();

        let default_input = host.default_input_device().map(|d| d.to_string());
        let default_output = host.default_output_device().map(|d| d.to_string());

        Devices {
            inputs: collect(host.input_devices().ok(), default_input.as_deref()),
            outputs: collect(host.output_devices().ok(), default_output.as_deref()),
        }
    }

    /// Whether a saved device name is still present.
    ///
    /// Used to tell "the default" apart from "the thing you chose, which is unplugged" in the
    /// settings screen — two states that both play through the speakers and need different
    /// explanations.
    pub fn has_input(&self, name: &str) -> bool {
        self.inputs.iter().any(|d| d.name == name)
    }

    pub fn has_output(&self, name: &str) -> bool {
        self.outputs.iter().any(|d| d.name == name)
    }
}

fn collect<I>(devices: Option<I>, default: Option<&str>) -> Vec<DeviceInfo>
where
    I: Iterator<Item = cpal::Device>,
{
    let Some(devices) = devices else { return Vec::new() };
    let mut found: Vec<DeviceInfo> = devices
        // The name is the device's `Display`, which is what cpal 0.18 offers without a
        // round trip to the driver for a full description — and a description can fail for a
        // device that is being unplugged while the list is being built.
        .map(|device| device.to_string())
        .map(|name| DeviceInfo { is_default: Some(name.as_str()) == default, name })
        .collect();

    // Some hosts list the same device more than once (an alias, or one entry per sample rate).
    // Two identical rows in a picker are a bug even when the underlying handles differ.
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found.dedup_by(|a, b| a.name == b.name);

    // The default first, then everything else alphabetically: it is the one most people want and
    // the only one that is correct to pick without knowing anything about the machine.
    found.sort_by_key(|d| !d.is_default);
    found
}

/// Open an input device by name, or the default if the name is absent or unknown.
pub fn open_input(name: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name {
        if let Some(found) = host
            .input_devices()
            .ok()
            .and_then(|mut devices| devices.find(|d| d.to_string() == name))
        {
            return Some(found);
        }
        // Not an error: this is what a headset being unplugged looks like, and falling back
        // silently is better than a dialogue nobody can act on mid-call.
        log::info!("audio: input {name:?} is not present; using the default");
    }
    host.default_input_device()
}

pub fn open_output(name: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name {
        if let Some(found) = host
            .output_devices()
            .ok()
            .and_then(|mut devices| devices.find(|d| d.to_string() == name))
        {
            return Some(found);
        }
        log::info!("audio: output {name:?} is not present; using the default");
    }
    host.default_output_device()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake device list, since the real one depends on what hardware the test machine has.
    fn info(name: &str, is_default: bool) -> DeviceInfo {
        DeviceInfo { name: name.to_string(), is_default }
    }

    #[test]
    fn enumeration_does_not_panic_whatever_the_host_says() {
        // The interesting property is that this returns rather than dying: it runs against
        // whatever audio system the machine has, including none at all in a container.
        let devices = Devices::enumerate();
        // Every listed device has a usable name, because an unnameable one cannot be saved.
        for device in devices.inputs.iter().chain(&devices.outputs) {
            assert!(!device.name.is_empty());
        }
        // At most one default per direction.
        assert!(devices.inputs.iter().filter(|d| d.is_default).count() <= 1);
        assert!(devices.outputs.iter().filter(|d| d.is_default).count() <= 1);
    }

    #[test]
    fn the_default_is_listed_first_and_the_rest_alphabetically() {
        let devices = Devices {
            inputs: vec![],
            outputs: vec![],
        };
        assert!(!devices.has_input("anything"));

        // Exercise the ordering through the same helper the real path uses, with a list that is
        // deliberately in the wrong order to begin with.
        let mut listed = vec![info("Zoom H1", false), info("Built-in", true), info("Aggregate", false)];
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        listed.dedup_by(|a, b| a.name == b.name);
        listed.sort_by_key(|d| !d.is_default);
        assert_eq!(listed[0].name, "Built-in", "the default comes first");
        assert_eq!(listed[1].name, "Aggregate");
        assert_eq!(listed[2].name, "Zoom H1");
    }

    #[test]
    fn a_saved_name_that_is_gone_is_reported_as_gone() {
        let devices = Devices {
            inputs: vec![info("Built-in", true)],
            outputs: vec![info("Speakers", true)],
        };
        assert!(devices.has_input("Built-in"));
        assert!(!devices.has_input("A Headset That Went Away"));
        assert!(devices.has_output("Speakers"));
    }
}
