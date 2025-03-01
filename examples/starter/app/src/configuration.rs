use pavex::blueprint::Blueprint;
use pavex::server::IncomingStream;
use pavex::t;

/// Refer to Pavex's [configuration guide](https://pavex.dev/docs/guide/configuration) for more details
/// on how to manage configuration values.
pub fn register(bp: &mut Blueprint) {
    bp.config("server", t!(self::ServerConfig));
    // Feel free to delete `GreetConfig` once you start working on your app!
    // It's here as a reference example on how to add a new configuration type.
    bp.config("greet", t!(self::GreetConfig));
}

#[derive(serde::Deserialize, Debug, Clone)]
/// Configuration for the HTTP server used to expose our API
/// to users.
pub struct ServerConfig {
    /// The port that the server must listen on.
    ///
    /// Set the `PX_SERVER__PORT` environment variable to override its value.
    #[serde(deserialize_with = "serde_aux::field_attributes::deserialize_number_from_string")]
    pub port: u16,
    /// The network interface that the server must be bound to.
    ///
    /// E.g. `0.0.0.0` for listening to incoming requests from
    /// all sources.
    ///
    /// Set the `PX_SERVER__IP` environment variable to override its value.
    pub ip: std::net::IpAddr,
    /// The timeout for graceful shutdown of the server.
    ///
    /// E.g. `1 minute` for a 1 minute timeout.
    ///
    /// Set the `PX_SERVER__GRACEFUL_SHUTDOWN_TIMEOUT` environment variable to override its value.
    #[serde(with = "humantime_serde")]
    pub graceful_shutdown_timeout: std::time::Duration,
}

impl ServerConfig {
    /// Bind a TCP listener according to the specified parameters.
    pub async fn listener(&self) -> Result<IncomingStream, std::io::Error> {
        let addr = std::net::SocketAddr::new(self.ip, self.port);
        IncomingStream::bind(addr).await
    }
}

#[derive(serde::Deserialize, Clone, Debug)]
/// A group of configuration values to showcase how app config works.
pub struct GreetConfig {
    /// Say "Hello {name}," rather than "Hello," in the response.
    pub use_name: bool,
    /// The message that's appended after the "Hello" line.
    pub greeting_message: String,
}
