/*
 * This file is part of rusty-pcap.
 *
 * rusty-pcap is free software: you can redistribute it and/or modify it under
 * the terms of the GNU General Public License as published by the Free Software
 * Foundation, either version 3 of the License, or (at your option) any later
 * version.
 *
 * rusty-pcap is distributed in the hope that it will be useful, but WITHOUT ANY
 * WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR
 * A PARTICULAR PURPOSE. See the GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License along with
 * rusty-pcap. If not, see <https://www.gnu.org/licenses/>.
 */

/*
 * This file contains the code that instantiates the rusty-pcap api.
 */

// Import necessary libraries and modules
use crate::packet_parse;
use crate::search_pcap;
use crate::search_pcap::parse_time_field;
use crate::write_pcap::{filter_to_name, pcap_to_write};
use crate::Config;
use crate::PcapFilter;
use lazy_static::lazy_static;
use pcap_file::pcap::PcapReader;
use rocket::fairing::{Fairing, Info, Kind};
use rocket::figment::Figment;
use rocket::fs::NamedFile;
use rocket::http::Status;
use rocket::request::{self, FromRequest};
use rocket::response::status::Custom;
use rocket::response::{self, Responder};
use rocket::{get, routes, Build, Request, Rocket};
use rocket_cors::{AllowedOrigins, CorsOptions};
use std::fs::File;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use std::sync::atomic::{AtomicU64, Ordering};
use tokio::task;

// Global variable to store the start time of the server
lazy_static! {
    static ref START_TIME: Mutex<Instant> = Mutex::new(Instant::now());
    static ref PCAP_RESPONSE_TIME_TOTAL: AtomicU64 = AtomicU64::new(0);
    static ref PCAP_REQUEST_COUNT: AtomicU64 = AtomicU64::new(0);
}

// Global variable to track number of requests to the server
static REQUEST_COUNT: AtomicU64 = AtomicU64::new(0);
struct CustomError(&'static str);
struct UptimeTracker;

#[rocket::async_trait]
impl Fairing for UptimeTracker {
    fn info(&self) -> Info {
        Info {
            name: "Uptime Tracker",
            kind: Kind::Ignite,
        }
    }

    async fn on_ignite(&self, rocket: Rocket<Build>) -> rocket::fairing::Result {
        *START_TIME.lock().unwrap() = Instant::now();
        Ok(rocket)
    }
}

struct RequestCounter;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for RequestCounter {
    type Error = ();

    async fn from_request(_request: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        REQUEST_COUNT.fetch_add(1, Ordering::Acquire);
        request::Outcome::Success(RequestCounter)
    }
}

impl<'r> Responder<'r, 'static> for CustomError {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        response::Response::build()
            .status(Status::BadRequest)
            .sized_body(self.0.len(), std::io::Cursor::new(self.0))
            .ok()
    }
}

pub async fn get_pcap(
    pcap_request: PcapFilter,
    config: &Config,
) -> Result<NamedFile, Custom<String>> {
    // create pcap file to write matched packet to
    let output_pcap_file = filter_to_name(&pcap_request);
    let mut pcap_writer = pcap_to_write(&pcap_request, config.output_directory.as_deref());
    // Start timer for how long the pcap search takes
    let start = Instant::now();
    let pcap_directory: Vec<String> = config
        .pcap_directory
        .as_ref()
        .unwrap()
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    // Check if timestamp is in RFC3339 or %Y-%m-%dT%H:%M:%S%.f%z format or a format we can parse
    // If timestamp isn't set default to 1970-01-01T00:00:00Z
    let flow_time = parse_time_field(
        pcap_request
            .timestamp
            .as_ref()
            .unwrap_or(&"1970-01-01T00:00:00".to_string()),
    );

    let search_time = match flow_time {
        Ok(time) => time,
        Err(_err) => {
            log::error!(
                "Timestamp {:?} is not in RFC3339 or %Y-%m-%dT%H:%M:%S%.f%z format",
                pcap_request.timestamp
            );
            return Err(Custom(
                Status::BadRequest,
                "Timestamp is not in RFC3339 or %Y-%m-%dT%H:%M:%S%.f%z format".to_string(),
            ));
        }
    };

    log::info!("Searching Pcap directory {:?}", &pcap_directory);

    let mut tasks = Vec::new();
    for dir in pcap_directory {
        let pcap_request = pcap_request.clone();
        //let search_time = search_time;
        tasks.push(task::spawn(async move {
            log::debug!(
                "Timestamp: {:?}",
                &pcap_request.timestamp.clone().unwrap_or_default()
            );

            log::debug!("Pcap Request: {:?}", pcap_request);
            search_pcap::directory(
                PathBuf::from(&dir),
                search_time,
                pcap_request.buffer.as_ref().unwrap_or(&"0".to_string()),
            )
            .unwrap_or_else(|err| {
                log::error!("Failed to get file list from directory: {:?}", &dir);
                log::error!("Error: {:?}", err);
                Vec::new()
            })
        }));
    }

    let mut file_list = Vec::new();
    for task in tasks {
        file_list.extend(task.await.unwrap());
    }
    log::info!("{:?} Pcap files to search", file_list.len());
    log::debug!("Files: {:?}", file_list);

    // look at every file
    for file in file_list {
        let path = file.as_path();
        match File::open(path) {
            Ok(file) => match PcapReader::new(file) {
                Ok(mut pcap_reader) => {
                    while let Some(Ok(packet)) = pcap_reader.next_packet() {
                        if packet_parse::packet_parse(&packet, &pcap_request) {
                            if let Err(err) = pcap_writer.write_packet(&packet) {
                                log::error!("Error writing packet to output pcap file: {}", err);
                            }
                        }
                    }
                }
                Err(_) => {
                    log::error!("Failed to create PcapReader for file: {:?}", path);
                }
            },
            Err(_) => {
                log::error!("Failed to open file: {:?}", path);
            }
        }
    }

    let duration = start.elapsed();
    log::info!("Pcap search took: {:?} seconds", duration.as_secs_f64());
    let file_path = PathBuf::from(
        config.output_directory.as_ref().unwrap().to_owned() + "/" + &output_pcap_file,
    );
    log::info!("Sending {:?} back to requestor.", file_path);
    NamedFile::open(file_path)
        .await
        .map_err(|_| Custom(Status::NotFound, "File not found".to_string()))
}

#[get("/")]
fn index() -> &'static str {
    "Welcome to the Rusty PCAP agent."
}

#[get("/pcap?<pcap_request..>")]
async fn pcap(
    pcap_request: Option<PcapFilter>,
    config: &rocket::State<Config>,
    _counter: RequestCounter,
) -> Result<NamedFile, Custom<String>> {
    let start_time = Instant::now();

    let result = match pcap_request {
        Some(request) => get_pcap(request, config.inner()).await,
        None => Err(Custom(
            Status::BadRequest,
            format!("Missing parameters: {:?}", pcap_request),
        )),
    };

    let response_time = start_time.elapsed().as_millis() as u64;
    PCAP_RESPONSE_TIME_TOTAL.fetch_add(response_time, Ordering::Relaxed);
    PCAP_REQUEST_COUNT.fetch_add(1, Ordering::Relaxed);

    result
}

#[get("/status")]
fn status() -> String {
    let start_time = START_TIME.lock().unwrap();
    let uptime_seconds = start_time.elapsed().as_secs();
    let request_count = REQUEST_COUNT.load(Ordering::Relaxed);
    let pcap_request_count = PCAP_REQUEST_COUNT.load(Ordering::Relaxed);
    let pcap_response_time_total = PCAP_RESPONSE_TIME_TOTAL.load(Ordering::Relaxed);
    let average_pcap_response_time = if pcap_request_count > 0 {
        pcap_response_time_total / pcap_request_count
    } else {
        0
    };

    let days = uptime_seconds / 86_400; // 60 * 60 * 24
    let hours = (uptime_seconds % 86_400) / 3_600; // (seconds % seconds_per_day) / seconds_per_hour
    let minutes = (uptime_seconds % 3_600) / 60; // (seconds % seconds_per_hour) / seconds_per_minute

    format!("Server uptime: {} days, {} hours, {} minutes\nTotal PCAPs served since start: {}\nAverage pcap response time: {} ms", days, hours, minutes, request_count, average_pcap_response_time)
}

pub fn rocket(config: crate::Config) -> rocket::Rocket<rocket::Build> {
    let mut figment = Figment::from(rocket::Config::figment());

    // if Cert and Key are set, use TLS
    if let (Some(cert), Some(key)) = (
        config.server.as_ref().unwrap().cert.as_ref(),
        config.server.as_ref().unwrap().key.as_ref(),
    ) {
        figment = figment.merge(("tls.certs", cert)).merge(("tls.key", key));
    } else {
        log::warn!("Cert or Key not set in config.toml, defaulting to http");
    }

    if let Some(address) = &config.server.as_ref().unwrap().address {
        figment = figment.merge(("address", address));
    } else {
        log::warn!("address not set in config.toml, defaulting to 127.0.0.1");
    }

    if let Some(port) = config.server.as_ref().unwrap().port {
        figment = figment.merge(("port", port));
    } else {
        log::warn!("Port not set in config.toml, defaulting to port 8000");
    }

    log::info!("Cors is {}", config.enable_cors);
    let cors = if config.enable_cors {
        CorsOptions {
            // Allow all origins
            allowed_origins: AllowedOrigins::all(),
            ..Default::default()
        }
        .to_cors()
        .expect("error while building CORS")
    } else {
        CorsOptions::default()
            .to_cors()
            .expect("error while building CORS")
    };

    rocket::custom(figment)
        .attach(UptimeTracker)
        .attach(cors)
        .manage(config)
        .mount("/", routes![pcap])
        .mount("/", routes![index])
        .mount("/", routes![status])
}
