#![allow(clippy::needless_update)]
use crate::Endpoint;

pub const CLASSIC_STYLE: u32 = 0;
pub const MODERN_STYLE: u32 = 1;

pub const DEFAULT_SERVERLIST_URL: &str =
    "https://raw.githubusercontent.com/rockisch/mhf-launcher/master/serverlist.json";

pub fn get_fixed_endpoints() -> Vec<Endpoint> {
    vec![Endpoint {
        name: "Localhost".into(),
        host: "localhost".into(),
        is_remote: true,
        ..Default::default()
    }]
}
