//! What a Server and a Relay say to each other.
//!
//! A Relay is known by its address, written one way ([`canonical_address`]).
//! A Server opens a WebSocket to its Relay at [`ENDPOINT_PATH`] beneath that
//! address and speaks first: it offers the versions of this protocol it speaks
//! and names its identity key, the Relay chooses a version and challenges it
//! to prove that key for that Relay alone, and from then on the Server is
//! known by the key alone (ADR-0048). Every message is one JSON text frame.
//!
//! This is the one piece of Suru held to compatibility on the wire
//! (ADR-0047). Once released it changes by addition, so everything here
//! tolerates fields and messages it does not recognize, where the Pairing's
//! own structures reject them. Until then it speaks only
//! [`Version::Unstable`] versions, which must match exactly, and it changes as
//! freely as the rest of Suru.

use std::{fmt, str::FromStr};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

mod proof;

pub use proof::{NONCE_LEN, ProofError, fingerprint, proof_message, supports_key, verify_proof};

/// Where beneath a Relay's address a Server opens its WebSocket.
pub const ENDPOINT_PATH: &str = "/connect";

/// The one way a Relay's address is written, by which a Server names the
/// Relay it proves its key to and a Relay knows its own: an `http` or `https`
/// URL naming a host — `https` where no scheme is given — with no credentials,
/// query or fragment, its host in lowercase, its scheme's default port left
/// out, and no trailing slash. `None` for anything no Relay is reached at.
pub fn canonical_address(address: &str) -> Option<String> {
    let address = address.trim();
    if address.is_empty() || address.chars().any(char::is_whitespace) {
        return None;
    }
    let address = if address.contains("://") {
        address.to_owned()
    } else {
        format!("https://{address}")
    };
    let url = url::Url::parse(&address).ok()?;
    let usable = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();
    usable.then(|| url.as_str().trim_end_matches('/').to_owned())
}

/// The versions of this protocol this build speaks.
pub const SPOKEN: &[Version] = &[Version::Unstable(1)];

/// A version of this protocol.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Version {
    /// One spoken before the protocol's first release. It agrees only with
    /// itself, and a later one is numbered higher.
    Unstable(u32),
    /// A released one, which later releases go on speaking for as long as
    /// Relays or Servers that old are in use.
    Stable(u32),
}

impl Version {
    /// Where this version stands among all of them: every unstable version
    /// before every released one.
    fn rank(self) -> (u8, u32) {
        match self {
            Self::Unstable(number) => (0, number),
            Self::Stable(number) => (1, number),
        }
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unstable(number) => write!(formatter, "unstable-{number}"),
            Self::Stable(number) => write!(formatter, "{number}"),
        }
    }
}

impl FromStr for Version {
    type Err = UnrecognizedVersion;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let number = |digits: &str| {
            (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| digits.parse().ok())
                .flatten()
                .ok_or(UnrecognizedVersion)
        };
        match text.strip_prefix("unstable-") {
            Some(digits) => number(digits).map(Self::Unstable),
            None => number(text).map(Self::Stable),
        }
    }
}

/// A version named in a shape this build cannot read: one of a later
/// release's, or nothing a version could be.
#[derive(Debug, Eq, PartialEq)]
pub struct UnrecognizedVersion;

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse()
            .map_err(|_| D::Error::custom(format!("unrecognized version `{text}`")))
    }
}

/// The versions named in a list, those this build cannot read left out, so a
/// later release offering a version of a shape not yet invented is still
/// understood in what it shares.
fn recognized_versions<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Version>, D::Error> {
    Ok(Vec::<String>::deserialize(deserializer)?
        .iter()
        .filter_map(|text| text.parse().ok())
        .collect())
}

/// One of the two sides of a connection to a Relay.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Server,
    Relay,
}

/// The version a Relay speaking `spoken` chooses from those a Server
/// `offered`: the latest both speak. Where they share none, the side that is
/// behind — the one whose latest version is the earlier — so that what refuses
/// the connection can say which side to upgrade.
pub fn agree(offered: &[Version], spoken: &[Version]) -> Result<Version, Side> {
    offered
        .iter()
        .filter(|version| spoken.contains(version))
        .max()
        .copied()
        .ok_or_else(|| match (offered.iter().max(), spoken.iter().max()) {
            (Some(offered), Some(spoken)) if offered > spoken => Side::Relay,
            _ => Side::Server,
        })
}

/// Bytes on the wire, as unpadded URL-safe base64.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Bytes(pub Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&URL_SAFE_NO_PAD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        URL_SAFE_NO_PAD
            .decode(text)
            .map(Self)
            .map_err(|_| D::Error::custom("bytes are not unpadded URL-safe base64"))
    }
}

/// What a Server says to a Relay.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The first thing a Server says: the versions it speaks, and its identity
    /// key — the DER SubjectPublicKeyInfo its Pairings pin — which it proves
    /// next.
    Hello {
        #[serde(deserialize_with = "recognized_versions")]
        versions: Vec<Version>,
        key: Bytes,
    },
    /// The answer to a [`RelayMessage::Challenge`]: the Server's signature,
    /// made with its identity key, over [`proof_message`].
    Proof { signature: Bytes },
    /// Begins a login for this Server's key, naming the Server by its
    /// machine's hostname for the Relay to label the Login with.
    BeginLogin { hostname: String },
    /// Asks the Relay to forget this Server's Login.
    Forget,
    /// A message of a later version this build does not know.
    #[serde(other)]
    Unrecognized,
}

/// What a Relay says to a Server.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayMessage {
    /// The version the Relay chose, the nonce the Server proves its key over,
    /// and the Relay's own canonical address, which the proof names. A Server
    /// answers only a challenge naming the address it knows the Relay at.
    Challenge {
        version: Version,
        nonce: Bytes,
        relay: String,
    },
    /// The Server proved its key. `login` is the Account its Login stands
    /// under, where it holds one that stands.
    Proven {
        #[serde(default)]
        login: Option<Account>,
    },
    /// A login has begun: the Server's user visits `verification_uri` on any
    /// device and enters `user_code` there before `expires_in_seconds` pass.
    LoginStarted {
        verification_uri: String,
        user_code: String,
        expires_in_seconds: u64,
    },
    /// The login is done, and the Server's Login stands under `account`.
    LoginDone { account: Account },
    /// The Relay has forgotten the Server's Login.
    Forgotten,
    /// What the Server asked is refused, for `refusal`; `message` says so to
    /// a reader.
    Refused { refusal: Refusal, message: String },
    /// A message of a later version this build does not know.
    #[serde(other)]
    Unrecognized,
}

/// The Account a Login stands under, as a Relay names it to the Server: by
/// its identity provider and the username it has there. The username is a
/// label only; the Relay knows the identity by the provider's stable subject
/// id, never by name.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Account {
    pub provider: String,
    pub username: String,
}

/// Why a Relay refused what a Server asked.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Refusal {
    /// The two share no version: `behind` is the side to upgrade, and
    /// `versions` those the Relay speaks.
    VersionNotSupported {
        #[serde(deserialize_with = "recognized_versions")]
        versions: Vec<Version>,
        behind: Side,
    },
    /// The Server's identity key is of a kind the Relay cannot check.
    UnsupportedKey,
    /// The Server's proof was not a signature over the challenge by the key it
    /// named.
    WrongProof,
    /// The Server's user refused the login at the identity provider.
    LoginDenied,
    /// Nobody finished the login before it expired.
    LoginExpired,
    /// The Relay could not log anyone in: its identity provider did not
    /// answer, or it has none.
    LoginUnavailable,
    /// The Server said something the Relay did not expect at that point, or
    /// did not recognize.
    Unexpected,
    /// A refusal of a later version this build does not know.
    #[serde(other)]
    Unrecognized,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_reads_as_it_is_written() {
        for (text, version) in [
            ("unstable-1", Version::Unstable(1)),
            ("unstable-12", Version::Unstable(12)),
            ("1", Version::Stable(1)),
            ("40", Version::Stable(40)),
        ] {
            assert_eq!(text.parse::<Version>(), Ok(version));
            assert_eq!(version.to_string(), text);
        }
        for text in [
            "",
            "unstable-",
            "unstable-x",
            "v1",
            "1.2",
            "-1",
            "+1",
            "unstable-+1",
        ] {
            assert_eq!(
                text.parse::<Version>(),
                Err(UnrecognizedVersion),
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_relay_is_known_by_its_address_written_one_way() {
        for (written, address) in [
            ("relay.example.com", "https://relay.example.com"),
            ("https://Relay.Example.com/", "https://relay.example.com"),
            ("https://relay.example.com:443", "https://relay.example.com"),
            ("  http://127.0.0.1:8080  ", "http://127.0.0.1:8080"),
            ("relay.example.com:8443", "https://relay.example.com:8443"),
            (
                "https://example.com/suru/relay/",
                "https://example.com/suru/relay",
            ),
        ] {
            assert_eq!(
                canonical_address(written).as_deref(),
                Some(address),
                "{written:?}"
            );
        }
    }

    #[test]
    fn an_address_no_relay_is_reached_at_has_no_canonical_form() {
        for written in [
            "",
            "   ",
            "ftp://relay.example.com",
            "wss://relay.example.com",
            "https://user:secret@relay.example.com",
            "https://relay.example.com/?token=1",
            "https://relay.example.com/#here",
            "https://relay example.com",
            "https://",
        ] {
            assert_eq!(canonical_address(written), None, "{written:?}");
        }
    }

    #[test]
    fn every_unstable_version_comes_before_every_released_one() {
        assert!(Version::Unstable(1) < Version::Unstable(2));
        assert!(Version::Unstable(900) < Version::Stable(1));
        assert!(Version::Stable(1) < Version::Stable(2));
    }

    #[test]
    fn the_latest_version_both_sides_speak_is_chosen() {
        assert_eq!(
            agree(
                &[Version::Stable(1), Version::Stable(2), Version::Stable(3)],
                &[Version::Stable(2), Version::Stable(1)],
            ),
            Ok(Version::Stable(2))
        );
        assert_eq!(
            agree(&[Version::Unstable(4)], &[Version::Unstable(4)]),
            Ok(Version::Unstable(4))
        );
    }

    #[test]
    fn unstable_versions_agree_only_with_themselves_and_a_mismatch_names_the_side_behind() {
        assert_eq!(
            agree(&[Version::Unstable(3)], &[Version::Unstable(4)]),
            Err(Side::Server)
        );
        assert_eq!(
            agree(&[Version::Unstable(5)], &[Version::Unstable(4)]),
            Err(Side::Relay)
        );
        assert_eq!(
            agree(&[Version::Stable(1)], &[Version::Unstable(4)]),
            Err(Side::Relay)
        );
        assert_eq!(agree(&[], &[Version::Unstable(4)]), Err(Side::Server));
    }

    #[test]
    fn what_either_side_says_tolerates_fields_it_does_not_recognize() {
        let hello: ServerMessage = serde_json::from_str(
            r#"{"type":"hello","versions":["unstable-1"],"key":"AAEC","later":{"field":1}}"#,
        )
        .expect("an unknown field is ignored");
        assert_eq!(
            hello,
            ServerMessage::Hello {
                versions: vec![Version::Unstable(1)],
                key: Bytes(vec![0, 1, 2]),
            }
        );
        let refused: RelayMessage = serde_json::from_str(
            r#"{"type":"refused","refusal":{"reason":"wrong_proof","detail":"x"},"message":"no","at":3}"#,
        )
        .expect("unknown fields at every level are ignored");
        assert_eq!(
            refused,
            RelayMessage::Refused {
                refusal: Refusal::WrongProof,
                message: "no".to_owned(),
            }
        );
        let proven: RelayMessage = serde_json::from_str(
            r#"{"type":"proven","login":{"provider":"github","username":"octo","id":7}}"#,
        )
        .unwrap();
        assert_eq!(
            proven,
            RelayMessage::Proven {
                login: Some(Account {
                    provider: "github".to_owned(),
                    username: "octo".to_owned(),
                }),
            }
        );
    }

    #[test]
    fn messages_and_refusals_of_a_later_version_are_recognized_as_unrecognized() {
        assert_eq!(
            serde_json::from_str::<ServerMessage>(r#"{"type":"wait_to_be_reached","x":1}"#)
                .unwrap(),
            ServerMessage::Unrecognized
        );
        assert_eq!(
            serde_json::from_str::<RelayMessage>(r#"{"type":"joined"}"#).unwrap(),
            RelayMessage::Unrecognized
        );
        assert_eq!(
            serde_json::from_str::<RelayMessage>(
                r#"{"type":"refused","refusal":{"reason":"cap_reached"},"message":"full"}"#
            )
            .unwrap(),
            RelayMessage::Refused {
                refusal: Refusal::Unrecognized,
                message: "full".to_owned(),
            }
        );
    }

    #[test]
    fn versions_offered_in_a_shape_not_yet_invented_are_left_out_rather_than_refused() {
        let hello: ServerMessage = serde_json::from_str(
            r#"{"type":"hello","versions":["3-preview","unstable-2","7"],"key":""}"#,
        )
        .unwrap();
        assert_eq!(
            hello,
            ServerMessage::Hello {
                versions: vec![Version::Unstable(2), Version::Stable(7)],
                key: Bytes(Vec::new()),
            }
        );
    }

    #[test]
    fn messages_survive_the_wire_unchanged() {
        for message in [
            ServerMessage::Hello {
                versions: SPOKEN.to_vec(),
                key: Bytes(vec![9; 91]),
            },
            ServerMessage::Proof {
                signature: Bytes(vec![1; 70]),
            },
            ServerMessage::BeginLogin {
                hostname: "workstation".to_owned(),
            },
            ServerMessage::Forget,
        ] {
            let wire = serde_json::to_string(&message).unwrap();
            assert_eq!(
                serde_json::from_str::<ServerMessage>(&wire).unwrap(),
                message
            );
        }
        for message in [
            RelayMessage::Challenge {
                version: Version::Unstable(1),
                nonce: Bytes(vec![7; NONCE_LEN]),
                relay: "https://relay.example.com".to_owned(),
            },
            RelayMessage::Proven { login: None },
            RelayMessage::LoginStarted {
                verification_uri: "https://example.com/device".to_owned(),
                user_code: "ABCD-1234".to_owned(),
                expires_in_seconds: 900,
            },
            RelayMessage::LoginDone {
                account: Account {
                    provider: "github".to_owned(),
                    username: "octo".to_owned(),
                },
            },
            RelayMessage::Forgotten,
            RelayMessage::Refused {
                refusal: Refusal::VersionNotSupported {
                    versions: SPOKEN.to_vec(),
                    behind: Side::Server,
                },
                message: "upgrade Suru".to_owned(),
            },
        ] {
            let wire = serde_json::to_string(&message).unwrap();
            assert_eq!(
                serde_json::from_str::<RelayMessage>(&wire).unwrap(),
                message
            );
        }
    }
}
