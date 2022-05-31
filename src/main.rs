#![feature(proc_macro_hygiene, decl_macro)]
use std::collections::HashMap;
use std::env;

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
struct AwsCreds {
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
struct MyState {
    token: String,
}

#[get("/<section>", format = "json")]
fn sections(
    section: &str,
    state: &State<MyState>,
) -> Result<Json<AwsCreds>, status::Unauthorized<&'static str>> {
    if state.token != "NONE" {
        return Err(status::Unauthorized(Some("Invalid token provided.")));
    }
    let data = get_ini_data(section);
    return Ok(Json(data));
}

#[get("/<section>?<token>", format = "json")]
fn sections_with_token(
    section: &str,
    token: &str,
    state: &State<MyState>,
) -> Result<Json<AwsCreds>, status::Unauthorized<&'static str>> {
    if state.token != "NONE" && state.token != token {
        return Err(status::Unauthorized(Some("Invalid token provided.")));
    }
    let data = get_ini_data(section);
    return Ok(Json(data));
}

#[get("/")]
fn index() -> &'static str {
    "Creds Provider - Hello!"
}

fn get_ini_data(section: &str) -> AwsCreds {
    let mut creds = HashMap::new();
    let path = format!("{}/.aws/credentials", dirs::home_dir().unwrap().display());
    let content = Ini::load_from_file(path).unwrap();

    for (sec, prop) in content.iter() {
        let mut aws_access_key_id = "";
        let mut aws_secret_access_key = "";
        let mut aws_security_token = "";

        for (k, v) in prop.iter() {
            if k == "aws_access_key_id" {
                aws_access_key_id = v;
            }
            if k == "aws_secret_access_key" {
                aws_secret_access_key = v;
            }
            if k == "aws_security_token" {
                aws_security_token = v;
            }
        }
        let data = AwsCreds {
            aws_access_key_id: aws_access_key_id.to_string(),
            aws_secret_access_key: aws_secret_access_key.to_string(),
            aws_security_token: aws_security_token.to_string(),
        };
        creds.insert(sec, data);
    }

    let search_type = Some(section);
    if creds.contains_key(&search_type) {
        let return_value = creds[&search_type].clone();
        return return_value;
    }

    let empty_aws_creds = AwsCreds::default();
    return empty_aws_creds;
}

#[launch]
fn rocket() -> _ {
    let _ = <Args as clap::CommandFactory>::command().print_help();
    let args = Args::parse();
    let config = MyState { token: args.token };

    env::set_var("ROCKET_PORT", args.port);
    rocket::build()
        .manage(config)
        .mount("/", routes![sections_with_token, sections, index])
}
