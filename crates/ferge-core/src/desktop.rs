//! What the desktop prefers, so ferge can blend in: its fonts.
//!
//! On Linux the preferences come from the XDG desktop portal, which every
//! major desktop and most compositors provide; GNOME's keys are the ones the
//! portal backends agree on. Other platforms report nothing here, because
//! their UI toolkit already resolves the system font itself.

use std::time::Duration;

use futures::stream::BoxStream;

/// How long to wait for the portal before going without it. D-Bus activation
/// of a missing portal would otherwise stall for its own 25 s timeout.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const PORTAL_TIMEOUT: Duration = Duration::from_secs(2);

/// A font as the desktop configured it.
#[derive(Clone, Debug, PartialEq)]
pub struct DesktopFont {
    pub family: String,
    /// Size in logical pixels, text scaling included.
    pub size: f32,
}

/// The desktop's interface and fixed-width fonts, as far as known.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DesktopFonts {
    pub interface: Option<DesktopFont>,
    pub monospace: Option<DesktopFont>,
}

/// Reads the desktop's fonts. Never fails: what cannot be read is `None`.
pub async fn fonts() -> DesktopFonts {
    #[cfg(target_os = "linux")]
    {
        match tokio::time::timeout(PORTAL_TIMEOUT, portal::fonts()).await {
            Ok(Ok(fonts)) => fonts,
            Ok(Err(error)) => {
                tracing::debug!(%error, "the desktop portal has no font settings");
                DesktopFonts::default()
            }
            Err(_) => {
                tracing::debug!("the desktop portal did not answer");
                DesktopFonts::default()
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        DesktopFonts::default()
    }
}

/// Yields whenever the desktop's fonts may have changed; read them again
/// with [`fonts`]. Ends at once where changes cannot be observed.
pub async fn font_changes() -> BoxStream<'static, ()> {
    #[cfg(target_os = "linux")]
    {
        match tokio::time::timeout(PORTAL_TIMEOUT, portal::font_changes()).await {
            Ok(Ok(changes)) => return changes,
            Ok(Err(error)) => tracing::debug!(%error, "cannot observe the desktop's fonts"),
            Err(_) => tracing::debug!("the desktop portal did not answer"),
        }
    }
    Box::pin(futures::stream::empty())
}

/// Style words that may follow the family in a Pango font description.
const PANGO_STYLE_WORDS: &[&str] = &[
    "normal",
    "roman",
    "oblique",
    "italic",
    "small-caps",
    "all-small-caps",
    "petite-caps",
    "all-petite-caps",
    "unicase",
    "title-caps",
    "ultra-condensed",
    "extra-condensed",
    "condensed",
    "semi-condensed",
    "semi-expanded",
    "expanded",
    "extra-expanded",
    "ultra-expanded",
    "thin",
    "ultra-light",
    "extra-light",
    "light",
    "semi-light",
    "demi-light",
    "book",
    "regular",
    "medium",
    "semi-bold",
    "demi-bold",
    "bold",
    "ultra-bold",
    "extra-bold",
    "heavy",
    "ultra-heavy",
    "black",
    "ultra-black",
    "extra-black",
];

/// Parses a Pango font description such as `Fira Sans 10`, `Cantarell Bold 11`
/// or `Noto Sans, Emoji 13px`. Sizes without a unit are points at 96 dpi;
/// `scale` is the desktop's text scaling factor.
pub fn parse_font_description(description: &str, scale: f64) -> Option<DesktopFont> {
    let mut words: Vec<&str> = description.split_whitespace().collect();
    let size = words
        .last()
        .and_then(|last| parse_size(last))
        .inspect(|_| {
            words.pop();
        })?;
    while words
        .last()
        .is_some_and(|word| PANGO_STYLE_WORDS.contains(&word.to_ascii_lowercase().as_str()))
    {
        words.pop();
    }
    let families = words.join(" ");
    let family = families
        .split(',')
        .map(str::trim)
        .find(|name| !name.is_empty())?;
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a font size fits an f32 with room to spare"
    )]
    let size = (size * scale) as f32;
    Some(DesktopFont {
        family: family.to_owned(),
        size,
    })
}

/// A size word in logical pixels: `10` (points) or `13px`.
fn parse_size(word: &str) -> Option<f64> {
    let (number, points) = match word.strip_suffix("px") {
        Some(pixels) => (pixels, false),
        None => (word, true),
    };
    let number: f64 = number.parse().ok()?;
    if !number.is_finite() || number <= 0.0 {
        return None;
    }
    Some(if points { number * 96.0 / 72.0 } else { number })
}

#[cfg(target_os = "linux")]
mod portal {
    use futures::StreamExt as _;
    use futures::stream::BoxStream;
    use zbus::zvariant::OwnedValue;

    use super::{DesktopFont, DesktopFonts, parse_font_description};

    const DESTINATION: &str = "org.freedesktop.portal.Desktop";
    const PATH: &str = "/org/freedesktop/portal/desktop";
    const INTERFACE: &str = "org.freedesktop.portal.Settings";
    const NAMESPACE: &str = "org.gnome.desktop.interface";
    const FONT_KEYS: [&str; 3] = ["font-name", "monospace-font-name", "text-scaling-factor"];

    async fn proxy() -> zbus::Result<zbus::Proxy<'static>> {
        let connection = zbus::Connection::session().await?;
        zbus::Proxy::new(&connection, DESTINATION, PATH, INTERFACE).await
    }

    pub(super) async fn fonts() -> zbus::Result<DesktopFonts> {
        let proxy = proxy().await?;
        let scale = read(&proxy, "text-scaling-factor")
            .await
            .and_then(|value| f64::try_from(value).ok())
            .unwrap_or(1.0);
        let font = async |key| -> Option<DesktopFont> {
            let description = String::try_from(read(&proxy, key).await?).ok()?;
            parse_font_description(&description, scale)
        };
        Ok(DesktopFonts {
            interface: font("font-name").await,
            monospace: font("monospace-font-name").await,
        })
    }

    /// One value, or `None` if the portal does not know it.
    async fn read(proxy: &zbus::Proxy<'static>, key: &str) -> Option<OwnedValue> {
        if let Ok(value) = proxy.call("ReadOne", &(NAMESPACE, key)).await {
            return Some(value);
        }
        // Portals before version 2 only offer `Read`, which wraps the value
        // in a second variant.
        let wrapped: OwnedValue = proxy.call("Read", &(NAMESPACE, key)).await.ok()?;
        match &*wrapped {
            zbus::zvariant::Value::Value(inner) => OwnedValue::try_from(&**inner).ok(),
            _ => Some(wrapped),
        }
    }

    pub(super) async fn font_changes() -> zbus::Result<BoxStream<'static, ()>> {
        let proxy = proxy().await?;
        let signals = proxy.receive_signal("SettingChanged").await?;
        let changes = signals.filter_map(move |message| {
            // The stream borrows nothing from the proxy, but the proxy keeps
            // the connection and its match rule alive.
            let _keep_alive = &proxy;
            let relevant = message
                .body()
                .deserialize::<(String, String, OwnedValue)>()
                .is_ok_and(|(namespace, key, _)| {
                    namespace == NAMESPACE && FONT_KEYS.contains(&key.as_str())
                });
            std::future::ready(relevant.then_some(()))
        });
        Ok(Box::pin(changes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(
        clippy::unnecessary_wraps,
        reason = "compared with what the parser returns"
    )]
    fn font(family: &str, size: f32) -> Option<DesktopFont> {
        Some(DesktopFont {
            family: family.to_owned(),
            size,
        })
    }

    #[test]
    fn parses_pango_descriptions() {
        let px = |points: f32| points * 96.0 / 72.0;
        assert_eq!(
            parse_font_description("Fira Sans 10", 1.0),
            font("Fira Sans", px(10.0))
        );
        assert_eq!(
            parse_font_description("Cantarell Bold 11", 1.0),
            font("Cantarell", px(11.0))
        );
        assert_eq!(
            parse_font_description("Noto Sans Semi-Bold Italic 9.5", 1.0),
            font("Noto Sans", px(9.5))
        );
        assert_eq!(
            parse_font_description("Inter, Noto Emoji 13px", 1.0),
            font("Inter", 13.0)
        );
        assert_eq!(
            parse_font_description("Fira Sans 10", 1.5),
            font("Fira Sans", px(15.0))
        );
    }

    #[test]
    fn rejects_descriptions_without_family_or_size() {
        assert_eq!(parse_font_description("Fira Sans", 1.0), None);
        assert_eq!(parse_font_description("11", 1.0), None);
        assert_eq!(parse_font_description("Bold 11", 1.0), None);
        assert_eq!(parse_font_description("Inter -3", 1.0), None);
        assert_eq!(parse_font_description("", 1.0), None);
    }

    #[test]
    fn ignores_a_broken_scale() {
        assert_eq!(
            parse_font_description("Inter 12px", f64::NAN),
            font("Inter", 12.0)
        );
        assert_eq!(
            parse_font_description("Inter 12px", 0.0),
            font("Inter", 12.0)
        );
    }
}
