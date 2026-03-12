#![allow(renamed_and_removed_lints)]

use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use aws_credential_types::Credentials;
use aws_sdk_sts::config::Region;
use aws_sdk_sts::Client;
use tokio::runtime::Runtime;

extern crate dirs;

#[macro_use]
extern crate rocket;
use rocket::form::FromForm;
use rocket::response::status;
use rocket::serde::json::Json;
use rocket::State;
use serde::{Deserialize, Serialize};

use clap::Parser;
use ini::Ini;
use log::{debug, info, warn};


#[derive(Parser)]
#[clap(
    name = "ini-as-rest",
    author = "Avner Cohen <israbirding@gmail.com>",
    about = "Serve AWS Credentials as local webserver, for Postman."
)]
struct Args {
    #[clap(short, long, default_value = "NONE")]
    token: String,
    #[clap(short, long, default_value = "9432")]
    port: String,
}

#[derive(Serialize, Deserialize, Default)]
pub struct AwsCreds {
    aws_access_key_id: String,
    aws_secret_access_key: String,
    aws_security_token: String,
}

impl Clone for AwsCreds {
    fn clone(&self) -> Self {
        Self {
            aws_access_key_id: self.aws_access_key_id.clone(),
            aws_secret_access_key: self.aws_secret_access_key.clone(),
            aws_security_token: self.aws_security_token.clone(),
        }
    }
}

#[derive(FromForm)]
pub struct MyState {
    pub token: String,
}

pub struct StsCache(Mutex<HashMap<String, (AwsCreds, SystemTime)>>);

#[get("/<section>", format = "json")]
fn sections(
    section: &str,
    state: &State<MyState>,
    cache: &State<StsCache>,
) -> Result<Json<AwsCreds>, status::Unauthorized<&'static str>> {
    if state.token != "NONE" {
        return Err(status::Unauthorized(Some("Invalid token provided.")));
    }
    let data = get_ini_data(section, cache);
    return Ok(Json(data));
}

#[get("/<section>?<token>", format = "json")]
fn sections_with_token(
    section: &str,
    token: &str,
    state: &State<MyState>,
    cache: &State<StsCache>,
) -> Result<Json<AwsCreds>, status::Unauthorized<&'static str>> {
    if state.token != "NONE" && state.token != token {
        return Err(status::Unauthorized(Some("Invalid token provided.")));
    }
    let data = get_ini_data(section, cache);
    return Ok(Json(data));
}

#[get("/")]
fn index() -> &'static str {
    "Creds Provider - Hello!"
}

struct ConfigProfile {
    role_arn: String,
    source_profile: String,
    region: Option<String>,
}

const CACHE_EXPIRY_BUFFER: Duration = Duration::from_secs(60);

fn get_ini_data(section: &str, cache: &StsCache) -> AwsCreds {
    info!("profile requested: {}", section);
    let path = format!("{}/.aws/credentials", dirs::home_dir().unwrap().display());
    debug!("credentials path: {}", path);
    let content = Ini::load_from_file(&path).unwrap_or_else(|_| Ini::new());
    let (creds, source_profiles, role_assumptions) = parse_credentials(&content);

    if let Some((role_arn, source_profile, region)) = role_assumptions.get(&Some(section.to_string())) {
        if let Some(cached) = get_cached(cache, section) {
            debug!("STS cache hit for '{}'", section);
            return cached;
        }
        info!("credentials: role_arn + source_profile for section '{}' -> assuming role (source_profile={})", section, source_profile);
        let mut visited = HashSet::new();
        let source_creds = resolve_creds(source_profile, &creds, &source_profiles, &mut visited);
        if !source_creds.aws_access_key_id.is_empty() {
            let region_str = region.as_deref().unwrap_or("us-east-1");
            debug!("calling STS AssumeRole role_arn={} region={}", role_arn, region_str);
            let (result, expires_at) = assume_role_blocking(&source_creds, role_arn, region_str);
            if !result.aws_access_key_id.is_empty() {
                set_cached(cache, section, &result, expires_at);
            }
            return result;
        }
        warn!("role assumption for '{}': source_profile '{}' resolved to empty creds", section, source_profile);
    }

    if let Some(config_profile) = load_config_profile(section) {
        if let Some(cached) = get_cached(cache, section) {
            debug!("STS cache hit for '{}' (config path)", section);
            return cached;
        }
        info!("config: role_arn + source_profile for '{}' (source_profile={})", section, config_profile.source_profile);
        let mut visited = HashSet::new();
        let source_creds = resolve_creds(&config_profile.source_profile, &creds, &source_profiles, &mut visited);
        if !source_creds.aws_access_key_id.is_empty() {
            let region_str = config_profile.region.as_deref().unwrap_or("us-east-1");
            let (result, expires_at) = assume_role_blocking(&source_creds, &config_profile.role_arn, region_str);
            if !result.aws_access_key_id.is_empty() {
                set_cached(cache, section, &result, expires_at);
            }
            return result;
        }
    }

    debug!("credentials: resolving '{}' via source_profile chain or direct", section);
    let mut visited = HashSet::new();
    let out = resolve_creds(section, &creds, &source_profiles, &mut visited);
    if out.aws_access_key_id.is_empty() {
        warn!("profile '{}': no credentials found", section);
    } else {
        info!("profile '{}': returned credentials (access_key_id prefix: {}...)", section, &out.aws_access_key_id[..out.aws_access_key_id.len().min(8)]);
    }
    out
}

fn get_cached(cache: &StsCache, section: &str) -> Option<AwsCreds> {
    let guard = cache.0.lock().ok()?;
    let (creds, expires_at) = guard.get(section)?;
    if SystemTime::now() + CACHE_EXPIRY_BUFFER < *expires_at {
        Some(creds.clone())
    } else {
        None
    }
}

fn set_cached(cache: &StsCache, section: &str, creds: &AwsCreds, expires_at: SystemTime) {
    if let Ok(mut guard) = cache.0.lock() {
        guard.insert(section.to_string(), (creds.clone(), expires_at));
        debug!("STS cached '{}' until {:?}", section, expires_at);
    }
}

fn load_config_profile(profile_name: &str) -> Option<ConfigProfile> {
    let config_path: PathBuf = dirs::home_dir()?.join(".aws/config");
    let content = Ini::load_from_file(config_path).ok()?;
    let expected_sec = if profile_name == "default" {
        "default".to_string()
    } else {
        format!("profile {}", profile_name)
    };
    for (sec, prop) in content.iter() {
        if sec.as_deref() != Some(expected_sec.as_str()) {
            continue;
        }
        let mut role_arn = None::<String>;
        let mut source_profile = None::<String>;
        let mut region = None::<String>;
        for (k, v) in prop.iter() {
            match k {
                "role_arn" => role_arn = Some(v.to_string()),
                "source_profile" => source_profile = Some(v.to_string()),
                "region" => region = Some(v.to_string()),
                _ => {}
            }
        }
        if let (Some(role_arn), Some(source_profile)) = (role_arn, source_profile) {
            return Some(ConfigProfile {
                role_arn,
                source_profile,
                region,
            });
        }
    }
    None
}

fn assume_role_blocking(source: &AwsCreds, role_arn: &str, region: &str) -> (AwsCreds, SystemTime) {
    info!("STS AssumeRole role_arn={} region={}", role_arn, region);
    let source = source.clone();
    let role_arn = role_arn.to_string();
    let region = region.to_string();
    std::thread::scope(|s| {
        s.spawn(|| {
            let rt = Runtime::new().expect("tokio runtime");
            rt.block_on(assume_role_async(source, role_arn, region))
        })
        .join()
        .expect("STS thread panicked")
    })
}

async fn assume_role_async(
    source: AwsCreds,
    role_arn: String,
    region: String,
) -> (AwsCreds, SystemTime) {
    let default_expiry = SystemTime::now() + Duration::from_secs(3600);
    let session_token = if source.aws_security_token.is_empty() {
        None
    } else {
        Some(source.aws_security_token.clone())
    };
    let creds = Credentials::new(
        &source.aws_access_key_id,
        &source.aws_secret_access_key,
        session_token,
        None,
        "ini-as-rest",
    );
    let config = aws_sdk_sts::Config::builder()
        .behavior_version(aws_sdk_sts::config::BehaviorVersion::latest())
        .credentials_provider(creds)
        .region(Region::new(region))
        .build();
    let client = Client::from_conf(config);
    let out = client
        .assume_role()
        .role_arn(&role_arn)
        .role_session_name("ini-as-rest")
        .send()
        .await;
    match out {
        Ok(resp) => {
            let c = resp.credentials().expect("AssumeRole returns credentials");
            let expires_at = SystemTime::try_from(c.expiration().clone()).unwrap_or(default_expiry);
            info!("STS AssumeRole success access_key_id={}... expires_at={:?}", &c.access_key_id()[..c.access_key_id().len().min(8)], expires_at);
            let creds = AwsCreds {
                aws_access_key_id: c.access_key_id().to_string(),
                aws_secret_access_key: c.secret_access_key().to_string(),
                aws_security_token: c.session_token().to_string(),
            };
            (creds, expires_at)
        }
        Err(e) => {
            warn!("STS AssumeRole failed: {:?}", e);
            (AwsCreds::default(), default_expiry)
        }
    }
}

fn parse_credentials(
    content: &Ini,
) -> (
    HashMap<Option<String>, AwsCreds>,
    HashMap<Option<String>, String>,
    HashMap<Option<String>, (String, String, Option<String>)>,
) {
    let mut creds = HashMap::new();
    let mut source_profiles = HashMap::new();
    let mut role_assumptions = HashMap::new();

    for (sec, prop) in content.iter() {
        let sec_key = sec.map(|s| s.to_string());
        let mut aws_access_key_id = "";
        let mut aws_secret_access_key = "";
        let mut aws_security_token = "";
        let mut source_profile = None::<&str>;
        let mut role_arn = None::<&str>;
        let mut region = None::<&str>;

        for (k, v) in prop.iter() {
            match k {
                "aws_access_key_id" => aws_access_key_id = v,
                "aws_secret_access_key" => aws_secret_access_key = v,
                "aws_security_token" => aws_security_token = v,
                "source_profile" => source_profile = Some(v),
                "role_arn" => role_arn = Some(v),
                "region" => region = Some(v),
                _ => {}
            }
        }
        creds.insert(
            sec_key.clone(),
            AwsCreds {
                aws_access_key_id: aws_access_key_id.to_string(),
                aws_secret_access_key: aws_secret_access_key.to_string(),
                aws_security_token: aws_security_token.to_string(),
            },
        );
        if let Some(sp) = source_profile {
            source_profiles.insert(sec_key.clone(), sp.to_string());
        }
        if let (Some(role_arn), Some(source_profile)) = (role_arn, source_profile) {
            role_assumptions.insert(
                sec_key,
                (
                    role_arn.to_string(),
                    source_profile.to_string(),
                    region.map(String::from),
                ),
            );
        }
    }

    (creds, source_profiles, role_assumptions)
}

fn resolve_creds(
    section: &str,
    creds: &HashMap<Option<String>, AwsCreds>,
    source_profiles: &HashMap<Option<String>, String>,
    visited: &mut HashSet<String>,
) -> AwsCreds {
    let key = Some(section.to_string());
    if !visited.insert(section.to_string()) {
        return AwsCreds::default();
    }

    if let Some(source_name) = source_profiles.get(&key) {
        debug!("resolve_creds: '{}' -> source_profile '{}'", section, source_name);
        let resolved = resolve_creds(source_name, creds, source_profiles, visited);
        visited.remove(section);
        return resolved;
    }

    visited.remove(section);
    creds.get(&key).cloned().unwrap_or_default()
}

#[launch]
fn rocket() -> _ {
    let _ = env_logger::try_init();
    let _ = <Args as clap::CommandFactory>::command().print_help();
    let args = Args::parse();
    let config = MyState { token: args.token };

    env::set_var("ROCKET_PORT", args.port);
    rocket::build()
        .manage(config)
        .manage(StsCache(Mutex::new(HashMap::new())))
        .mount("/", routes![sections_with_token, sections, index])
}
