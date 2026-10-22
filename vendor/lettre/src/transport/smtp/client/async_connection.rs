use std::{fmt, fmt::Display, io, net::IpAddr, time::Duration};

use futures_util::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[cfg(feature = "tokio1")]
use super::async_net::AsyncTokioStream;
#[cfg(feature = "tracing")]
use super::escape_crlf;
#[allow(deprecated)]
use super::{
    ClientCodec, MAX_RESPONSE_BYTES, MAX_RESPONSE_LINE_BYTES, TlsParameters,
    async_net::AsyncNetworkStream,
};
use crate::{
    Envelope,
    transport::smtp::{
        authentication::{Credentials, Mechanism},
        commands::{Auth, Data, Ehlo, Mail, Noop, Quit, Rcpt, Starttls},
        error,
        error::Error,
        extension::{ClientId, Extension, MailBodyParameter, MailParameter, ServerInfo},
        response::{Response, Severity, parse_response},
    },
};

macro_rules! try_smtp (
    ($err: expr, $client: ident) => ({
        match $err {
            Ok(val) => val,
            Err(err) => {
                $client.abort().await;
                return Err(From::from(err))
            },
        }
    })
);

/// Structure that implements the SMTP client
pub struct AsyncSmtpConnection {
    /// TCP stream between client and server
    /// Value is None before connection
    #[allow(deprecated)]
    stream: BufReader<AsyncNetworkStream>,
    /// Panic state
    panic: bool,
    /// Information about the server
    server_info: ServerInfo,
}

/// The phase of an SMTP DATA transaction that failed.
#[cfg(feature = "tokio1")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AsyncSmtpDataPhase {
    /// Writing encoded message content. Failure is known to precede the DATA terminator.
    Content,
    /// Writing the DATA terminator. Failure is ambiguous because the server may have received it.
    Terminator,
    /// Waiting for the server's final response after the DATA terminator.
    FinalResponse,
}

/// An error from a phase-aware SMTP DATA transaction.
#[cfg(feature = "tokio1")]
#[derive(Debug)]
pub struct AsyncSmtpDataError {
    phase: AsyncSmtpDataPhase,
    kind: AsyncSmtpDataErrorKind,
}

/// Timeouts for establishing an SMTP connection and completing its greeting exchange.
#[cfg(feature = "tokio1")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AsyncSmtpConnectTimeouts {
    /// Local safety limit for DNS resolution and TCP connection establishment.
    pub connect: Duration,
    /// Local safety limit for an implicit TLS handshake.
    pub tls_handshake: Duration,
    /// Maximum wait for the server's initial 220 greeting.
    pub greeting: Duration,
    /// Maximum wait for the initial EHLO exchange.
    pub ehlo: Duration,
}

/// Timeouts for upgrading an established SMTP connection with STARTTLS.
#[cfg(feature = "tokio1")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AsyncSmtpStartTlsTimeouts {
    /// Maximum wait for the STARTTLS command exchange.
    pub command: Duration,
    /// Local safety limit for the TLS handshake.
    pub tls_handshake: Duration,
    /// Maximum wait for the required post-TLS EHLO exchange.
    pub ehlo: Duration,
}

#[cfg(feature = "tokio1")]
#[derive(Debug)]
enum AsyncSmtpDataErrorKind {
    Timeout,
    Smtp(Error),
}

#[cfg(feature = "tokio1")]
impl AsyncSmtpDataError {
    fn timeout(phase: AsyncSmtpDataPhase) -> Self {
        Self {
            phase,
            kind: AsyncSmtpDataErrorKind::Timeout,
        }
    }

    fn smtp(phase: AsyncSmtpDataPhase, error: Error) -> Self {
        Self {
            phase,
            kind: AsyncSmtpDataErrorKind::Smtp(error),
        }
    }

    /// Returns the DATA phase in which the failure occurred.
    pub fn phase(&self) -> AsyncSmtpDataPhase {
        self.phase
    }

    /// Returns true if the configured phase timeout elapsed.
    pub fn is_timeout(&self) -> bool {
        matches!(self.kind, AsyncSmtpDataErrorKind::Timeout)
    }

    /// Returns the SMTP error when the failure came from I/O or a server response.
    pub fn smtp_error(&self) -> Option<&Error> {
        match &self.kind {
            AsyncSmtpDataErrorKind::Timeout => None,
            AsyncSmtpDataErrorKind::Smtp(error) => Some(error),
        }
    }
}

#[cfg(feature = "tokio1")]
impl fmt::Display for AsyncSmtpDataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            AsyncSmtpDataErrorKind::Timeout => {
                write!(f, "SMTP DATA {:?} phase timed out", self.phase)
            }
            AsyncSmtpDataErrorKind::Smtp(error) => {
                write!(f, "SMTP DATA {:?} phase failed: {error}", self.phase)
            }
        }
    }
}

#[cfg(feature = "tokio1")]
impl std::error::Error for AsyncSmtpDataError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.smtp_error().map(|error| {
            let source: &(dyn std::error::Error + 'static) = error;
            source
        })
    }
}

#[cfg(feature = "tokio1")]
async fn smtp_timeout<T>(
    duration: Duration,
    phase: &'static str,
    future: impl std::future::Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    match tokio1_crate::time::timeout(duration, future).await {
        Ok(result) => result,
        Err(_) => Err(error::network(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("SMTP {phase} timed out"),
        ))),
    }
}

#[cfg(feature = "tokio1")]
#[allow(deprecated)]
async fn connect_network_with_timeout(
    duration: Duration,
    future: impl std::future::Future<Output = Result<AsyncNetworkStream, Error>>,
) -> Result<AsyncNetworkStream, Error> {
    smtp_timeout(duration, "DNS/TCP connection", future).await
}

#[cfg(feature = "tokio1")]
#[allow(deprecated)]
async fn upgrade_implicit_tls_with_timeout(
    stream: &mut AsyncNetworkStream,
    tls_parameters: TlsParameters,
    duration: Duration,
) -> Result<(), Error> {
    smtp_timeout(
        duration,
        "implicit TLS handshake",
        stream.upgrade_tls(tls_parameters),
    )
    .await
}

impl AsyncSmtpConnection {
    /// Get information about the server
    pub fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    /// Connects with existing async stream
    ///
    /// Sends EHLO and parses server information
    #[cfg(feature = "tokio1")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio1")))]
    pub async fn connect_with_transport(
        stream: Box<dyn AsyncTokioStream>,
        hello_name: &ClientId,
    ) -> Result<AsyncSmtpConnection, Error> {
        #[allow(deprecated)]
        let stream = AsyncNetworkStream::use_existing_tokio1(stream);
        Self::connect_impl(stream, hello_name).await
    }

    /// Connects to the configured server
    ///
    /// If `tls_parameters` is `Some`, then the connection will use Implicit TLS (sometimes
    /// referred to as `SMTPS`). See also [`AsyncSmtpConnection::starttls`].
    ///
    /// If `local_address` is `Some`, then the address provided shall be used to bind the
    /// connection to a specific local address using [`tokio1_crate::net::TcpSocket::bind`].
    ///
    /// Sends EHLO and parses server information
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # use lettre::transport::smtp::{client::{AsyncSmtpConnection, TlsParameters}, extension::ClientId};
    /// # use tokio1_crate::{self as tokio, net::ToSocketAddrs as _};
    /// #
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let connection = AsyncSmtpConnection::connect_tokio1(
    ///     ("example.com", 465),
    ///     Some(Duration::from_secs(60)),
    ///     &ClientId::default(),
    ///     Some(TlsParameters::new("example.com".to_owned())?),
    ///     None,
    /// )
    /// .await?;
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "tokio1")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio1")))]
    pub async fn connect_tokio1<T: tokio1_crate::net::ToSocketAddrs>(
        server: T,
        timeout: Option<Duration>,
        hello_name: &ClientId,
        tls_parameters: Option<TlsParameters>,
        local_address: Option<IpAddr>,
    ) -> Result<AsyncSmtpConnection, Error> {
        #[allow(deprecated)]
        let stream =
            AsyncNetworkStream::connect_tokio1(server, timeout, tls_parameters, local_address)
                .await?;
        Self::connect_impl(stream, hello_name).await
    }

    /// Connects with independent DNS/TCP, TLS, greeting, and EHLO timeouts.
    ///
    /// This leaves the existing [`AsyncSmtpConnection::connect_tokio1`] behavior unchanged.
    #[cfg(feature = "tokio1")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio1")))]
    pub async fn connect_tokio1_with_timeouts<T: tokio1_crate::net::ToSocketAddrs>(
        server: T,
        timeouts: AsyncSmtpConnectTimeouts,
        hello_name: &ClientId,
        tls_parameters: Option<TlsParameters>,
        local_address: Option<IpAddr>,
    ) -> Result<AsyncSmtpConnection, Error> {
        #[allow(deprecated)]
        let mut stream = connect_network_with_timeout(
            timeouts.connect,
            AsyncNetworkStream::connect_tokio1(server, Some(timeouts.connect), None, local_address),
        )
        .await?;

        if let Some(tls_parameters) = tls_parameters {
            upgrade_implicit_tls_with_timeout(&mut stream, tls_parameters, timeouts.tls_handshake)
                .await?;
        }

        Self::connect_impl_with_timeouts(stream, hello_name, timeouts).await
    }

    /// Connects to the configured server
    ///
    /// Sends EHLO and parses server information
    #[cfg(feature = "async-std1")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async-std1")))]
    pub async fn connect_asyncstd1<T: async_std::net::ToSocketAddrs>(
        server: T,
        timeout: Option<Duration>,
        hello_name: &ClientId,
        tls_parameters: Option<TlsParameters>,
    ) -> Result<AsyncSmtpConnection, Error> {
        #[allow(deprecated)]
        let stream = AsyncNetworkStream::connect_asyncstd1(server, timeout, tls_parameters).await?;
        Self::connect_impl(stream, hello_name).await
    }

    #[allow(deprecated)]
    async fn connect_impl(
        stream: AsyncNetworkStream,
        hello_name: &ClientId,
    ) -> Result<AsyncSmtpConnection, Error> {
        let stream = BufReader::new(stream);
        let mut conn = AsyncSmtpConnection {
            stream,
            panic: false,
            server_info: ServerInfo::default(),
        };
        // TODO log
        let _response = conn.read_response().await?;

        conn.ehlo(hello_name).await?;

        // Print server information
        #[cfg(feature = "tracing")]
        tracing::debug!("server {}", conn.server_info);
        Ok(conn)
    }

    #[cfg(feature = "tokio1")]
    #[allow(deprecated)]
    async fn connect_impl_with_timeouts(
        stream: AsyncNetworkStream,
        hello_name: &ClientId,
        timeouts: AsyncSmtpConnectTimeouts,
    ) -> Result<AsyncSmtpConnection, Error> {
        let stream = BufReader::new(stream);
        let mut conn = AsyncSmtpConnection {
            stream,
            panic: false,
            server_info: ServerInfo::default(),
        };

        let greeting =
            match smtp_timeout(timeouts.greeting, "server greeting", conn.read_response()).await {
                Ok(greeting) => greeting,
                Err(error) => {
                    conn.panic = true;
                    return Err(error);
                }
            };
        if !greeting.has_code(220) {
            conn.panic = true;
            return Err(error::response(format!(
                "unexpected SMTP greeting response: {}",
                greeting.code()
            )));
        }

        conn.ehlo_with_timeout(hello_name, timeouts.ehlo).await?;

        #[cfg(feature = "tracing")]
        tracing::debug!("server {}", conn.server_info);
        Ok(conn)
    }

    pub async fn send(&mut self, envelope: &Envelope, email: &[u8]) -> Result<Response, Error> {
        // Mail
        let mut mail_options = vec![];

        // Internationalization handling
        //
        // * 8BITMIME: https://tools.ietf.org/html/rfc6152
        // * SMTPUTF8: https://tools.ietf.org/html/rfc653

        // Check for non-ascii addresses and use the SMTPUTF8 option if any.
        if envelope.has_non_ascii_addresses() {
            if !self.server_info().supports_feature(Extension::SmtpUtfEight) {
                // don't try to send non-ascii addresses (per RFC)
                return Err(error::client(
                    "Envelope contains non-ascii chars but server does not support SMTPUTF8",
                ));
            }
            mail_options.push(MailParameter::SmtpUtfEight);
        }

        // Check for non-ascii content in the message
        if !email.is_ascii() {
            if !self.server_info().supports_feature(Extension::EightBitMime) {
                return Err(error::client(
                    "Message contains non-ascii chars but server does not support 8BITMIME",
                ));
            }
            mail_options.push(MailParameter::Body(MailBodyParameter::EightBitMime));
        }

        try_smtp!(
            self.command(Mail::new(envelope.from().cloned(), mail_options))
                .await,
            self
        );

        // Recipient
        for to_address in envelope.to() {
            try_smtp!(
                self.command(Rcpt::new(to_address.clone(), vec![])).await,
                self
            );
        }

        // Data
        try_smtp!(self.command(Data).await, self);

        // Message content
        let result = try_smtp!(self.message(email).await, self);
        Ok(result)
    }

    pub fn has_broken(&self) -> bool {
        self.panic
    }

    pub fn can_starttls(&self) -> bool {
        !self.is_encrypted() && self.server_info.supports_feature(Extension::StartTls)
    }

    /// Upgrade the connection using `STARTTLS`.
    ///
    /// As described in [rfc3207]. Note that this mechanism has been deprecated in [rfc8314].
    ///
    /// [rfc3207]: https://www.rfc-editor.org/rfc/rfc3207
    /// [rfc8314]: https://www.rfc-editor.org/rfc/rfc8314
    #[allow(unused_variables)]
    pub async fn starttls(
        &mut self,
        tls_parameters: TlsParameters,
        hello_name: &ClientId,
    ) -> Result<(), Error> {
        if self.server_info.supports_feature(Extension::StartTls) {
            try_smtp!(self.command(Starttls).await, self);
            self.stream.get_mut().upgrade_tls(tls_parameters).await?;
            #[cfg(feature = "tracing")]
            tracing::debug!("connection encrypted");
            // Send EHLO again
            try_smtp!(self.ehlo(hello_name).await, self);
            Ok(())
        } else {
            Err(error::client("STARTTLS is not supported on this server"))
        }
    }

    /// Upgrades the connection with independent STARTTLS, handshake, and EHLO timeouts.
    #[cfg(feature = "tokio1")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio1")))]
    pub async fn starttls_with_timeouts(
        &mut self,
        tls_parameters: TlsParameters,
        hello_name: &ClientId,
        timeouts: AsyncSmtpStartTlsTimeouts,
    ) -> Result<(), Error> {
        if !self.server_info.supports_feature(Extension::StartTls) {
            return Err(error::client("STARTTLS is not supported on this server"));
        }

        let response = self
            .command_with_named_timeout(Starttls, timeouts.command, "STARTTLS command")
            .await?;
        if !response.has_code(220) {
            self.panic = true;
            return Err(error::response(format!(
                "unexpected STARTTLS response: {}",
                response.code()
            )));
        }

        if let Err(error) = smtp_timeout(
            timeouts.tls_handshake,
            "STARTTLS handshake",
            self.stream.get_mut().upgrade_tls(tls_parameters),
        )
        .await
        {
            self.panic = true;
            return Err(error);
        }

        #[cfg(feature = "tracing")]
        tracing::debug!("connection encrypted");

        self.ehlo_with_timeout(hello_name, timeouts.ehlo).await
    }

    /// Send EHLO and update server info
    async fn ehlo(&mut self, hello_name: &ClientId) -> Result<(), Error> {
        let ehlo_response = try_smtp!(self.command(Ehlo::new(hello_name.clone())).await, self);
        self.server_info = try_smtp!(ServerInfo::from_response(&ehlo_response), self);
        Ok(())
    }

    #[cfg(feature = "tokio1")]
    async fn ehlo_with_timeout(
        &mut self,
        hello_name: &ClientId,
        timeout: Duration,
    ) -> Result<(), Error> {
        let ehlo_response = self
            .command_with_named_timeout(Ehlo::new(hello_name.clone()), timeout, "EHLO command")
            .await?;
        if !ehlo_response.has_code(250) {
            self.panic = true;
            return Err(error::response(format!(
                "unexpected EHLO response: {}",
                ehlo_response.code()
            )));
        }
        self.server_info = match ServerInfo::from_response(&ehlo_response) {
            Ok(server_info) => server_info,
            Err(error) => {
                self.panic = true;
                return Err(error);
            }
        };
        Ok(())
    }

    pub async fn quit(&mut self) -> Result<Response, Error> {
        Ok(try_smtp!(self.command(Quit).await, self))
    }

    pub async fn abort(&mut self) {
        // Only try to quit if we are not already broken
        if !self.panic {
            self.panic = true;
            let _ = self.command(Quit).await;
        }
        let _ = self.stream.close().await;
    }

    /// Sets the underlying stream
    #[allow(deprecated)]
    pub fn set_stream(&mut self, stream: AsyncNetworkStream) {
        self.stream = BufReader::new(stream);
    }

    /// Tells if the underlying stream is currently encrypted
    pub fn is_encrypted(&self) -> bool {
        self.stream.get_ref().is_encrypted()
    }

    /// Checks if the server is connected using the NOOP SMTP command
    pub async fn test_connected(&mut self) -> bool {
        self.command(Noop).await.is_ok()
    }

    /// Sends an AUTH command with the given mechanism, and handles the challenge if needed
    pub async fn auth(
        &mut self,
        mechanisms: &[Mechanism],
        credentials: &Credentials,
    ) -> Result<Response, Error> {
        let mechanism = self
            .server_info
            .get_auth_mechanism(mechanisms)
            .ok_or_else(|| error::client("No compatible authentication mechanism was found"))?;

        // Limit challenges to avoid blocking
        let mut challenges: u8 = 10;
        let mut response = self
            .command(Auth::new(mechanism, credentials.clone(), None)?)
            .await?;

        while challenges > 0 && response.has_code(334) {
            challenges -= 1;
            response = try_smtp!(
                self.command(Auth::new_from_response(
                    mechanism,
                    credentials.clone(),
                    &response,
                )?)
                .await,
                self
            );
        }

        if challenges == 0 {
            Err(error::response("Unexpected number of challenges"))
        } else {
            Ok(response)
        }
    }

    /// Authenticates with a fresh timeout for the initial command and every challenge.
    #[cfg(feature = "tokio1")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio1")))]
    pub async fn auth_with_timeout(
        &mut self,
        mechanisms: &[Mechanism],
        credentials: &Credentials,
        command_timeout: Duration,
    ) -> Result<Response, Error> {
        let mechanism = self
            .server_info
            .get_auth_mechanism(mechanisms)
            .ok_or_else(|| error::client("No compatible authentication mechanism was found"))?;

        let mut challenges: u8 = 10;
        let mut response = self
            .command_with_named_timeout(
                Auth::new(mechanism, credentials.clone(), None)?,
                command_timeout,
                "AUTH command",
            )
            .await?;

        while challenges > 0 && response.has_code(334) {
            challenges -= 1;
            let command = match Auth::new_from_response(mechanism, credentials.clone(), &response) {
                Ok(command) => command,
                Err(error) => {
                    self.panic = true;
                    return Err(error);
                }
            };
            response = self
                .command_with_named_timeout(command, command_timeout, "AUTH challenge")
                .await?;
        }

        if challenges == 0 {
            self.panic = true;
            Err(error::response("Unexpected number of challenges"))
        } else if !response.has_code(235) {
            self.panic = true;
            Err(error::response(format!(
                "unexpected AUTH response: {}",
                response.code()
            )))
        } else {
            Ok(response)
        }
    }

    /// Sends the message content
    pub async fn message(&mut self, message: &[u8]) -> Result<Response, Error> {
        self.message_iter(std::iter::once(message)).await
    }

    /// Sends the message content by consuming an iterator that in its whole represents a message.
    pub async fn message_iter<I, B>(&mut self, message: I) -> Result<Response, Error>
    where
        I: Iterator<Item = B>,
        B: AsRef<[u8]>,
    {
        let mut codec = ClientCodec::new();
        for message_part in message {
            let message_part = message_part.as_ref();
            let mut out_buf = Vec::with_capacity(message_part.len());
            codec.encode(message_part, &mut out_buf);
            self.write(out_buf.as_slice()).await?;
        }
        self.write(b"\r\n.\r\n").await?;

        self.read_response().await
    }

    /// Sends message content with independent progress, terminator, and final-response timeouts.
    ///
    /// `content_chunk_size` bounds how much source content is encoded before each progress-timed
    /// write. The progress timer restarts after every successful write to the underlying stream.
    #[cfg(feature = "tokio1")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio1")))]
    pub async fn message_with_timeouts(
        &mut self,
        message: &[u8],
        content_chunk_size: usize,
        content_progress_timeout: Duration,
        terminator_timeout: Duration,
        final_response_timeout: Duration,
    ) -> Result<Response, AsyncSmtpDataError> {
        let result = self
            .message_with_timeouts_impl(
                message,
                content_chunk_size,
                content_progress_timeout,
                terminator_timeout,
                final_response_timeout,
            )
            .await;
        if result.is_err() {
            self.panic = true;
        }
        result
    }

    #[cfg(feature = "tokio1")]
    async fn message_with_timeouts_impl(
        &mut self,
        message: &[u8],
        content_chunk_size: usize,
        content_progress_timeout: Duration,
        terminator_timeout: Duration,
        final_response_timeout: Duration,
    ) -> Result<Response, AsyncSmtpDataError> {
        if content_chunk_size == 0 {
            return Err(AsyncSmtpDataError::smtp(
                AsyncSmtpDataPhase::Content,
                error::client("SMTP DATA content chunk size must be greater than zero"),
            ));
        }

        let mut codec = ClientCodec::new();
        for message_part in message.chunks(content_chunk_size) {
            let mut out_buf = Vec::with_capacity(message_part.len());
            codec.encode(message_part, &mut out_buf);
            self.write_with_progress_timeout(
                out_buf.as_slice(),
                content_progress_timeout,
                AsyncSmtpDataPhase::Content,
            )
            .await?;
        }

        self.write_with_progress_timeout(
            b"\r\n.\r\n",
            terminator_timeout,
            AsyncSmtpDataPhase::Terminator,
        )
        .await?;

        match tokio1_crate::time::timeout(final_response_timeout, self.read_response()).await {
            Ok(Ok(response)) if response.code().severity == Severity::PositiveCompletion => {
                Ok(response)
            }
            Ok(Ok(response)) => Err(AsyncSmtpDataError::smtp(
                AsyncSmtpDataPhase::FinalResponse,
                error::response(format!(
                    "unexpected intermediate SMTP response after DATA terminator: {}",
                    response.code()
                )),
            )),
            Ok(Err(error)) => Err(AsyncSmtpDataError::smtp(
                AsyncSmtpDataPhase::FinalResponse,
                error,
            )),
            Err(_) => Err(AsyncSmtpDataError::timeout(
                AsyncSmtpDataPhase::FinalResponse,
            )),
        }
    }

    #[cfg(feature = "tokio1")]
    async fn write_with_progress_timeout(
        &mut self,
        mut bytes: &[u8],
        progress_timeout: Duration,
        phase: AsyncSmtpDataPhase,
    ) -> Result<(), AsyncSmtpDataError> {
        use futures_util::io::AsyncWriteExt as _;

        while !bytes.is_empty() {
            let write = self.stream.get_mut().write(bytes);
            let written = match tokio1_crate::time::timeout(progress_timeout, write).await {
                Ok(Ok(written)) => written,
                Ok(Err(error)) => {
                    return Err(AsyncSmtpDataError::smtp(phase, error::network(error)));
                }
                Err(_) => return Err(AsyncSmtpDataError::timeout(phase)),
            };
            if written == 0 {
                return Err(AsyncSmtpDataError::smtp(
                    phase,
                    error::network(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "failed to make progress writing SMTP DATA",
                    )),
                ));
            }
            bytes = &bytes[written..];
        }

        match tokio1_crate::time::timeout(progress_timeout, self.stream.get_mut().flush()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(AsyncSmtpDataError::smtp(phase, error::network(error)));
            }
            Err(_) => return Err(AsyncSmtpDataError::timeout(phase)),
        }

        #[cfg(feature = "tracing")]
        tracing::debug!("Wrote SMTP DATA {:?} phase bytes", phase);
        Ok(())
    }

    /// Sends an SMTP command
    pub async fn command<C: Display>(&mut self, command: C) -> Result<Response, Error> {
        self.write(command.to_string().as_bytes()).await?;
        self.read_response().await
    }

    /// Sends one SMTP command with a fresh timeout for its complete exchange.
    #[cfg(feature = "tokio1")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio1")))]
    pub async fn command_with_timeout<C: Display>(
        &mut self,
        command: C,
        timeout: Duration,
    ) -> Result<Response, Error> {
        self.command_with_named_timeout(command, timeout, "command")
            .await
    }

    #[cfg(feature = "tokio1")]
    async fn command_with_named_timeout<C: Display>(
        &mut self,
        command: C,
        timeout: Duration,
        phase: &'static str,
    ) -> Result<Response, Error> {
        match smtp_timeout(timeout, phase, self.command(command)).await {
            Ok(response) => Ok(response),
            Err(error) => {
                self.panic = true;
                Err(error)
            }
        }
    }

    /// Writes a string to the server
    async fn write(&mut self, string: &[u8]) -> Result<(), Error> {
        self.stream
            .get_mut()
            .write_all(string)
            .await
            .map_err(error::network)?;
        self.stream
            .get_mut()
            .flush()
            .await
            .map_err(error::network)?;

        #[cfg(feature = "tracing")]
        tracing::debug!("Wrote: {}", escape_crlf(&String::from_utf8_lossy(string)));
        Ok(())
    }

    /// Gets the SMTP response
    pub async fn read_response(&mut self) -> Result<Response, Error> {
        let mut buffer = String::with_capacity(100);
        let mut pre = 0;

        while self
            .stream
            .read_line(&mut buffer)
            .await
            .map_err(error::network)?
            > 0
        {
            if buffer.len() - pre > MAX_RESPONSE_LINE_BYTES {
                return Err(error::response("SMTP response line too long"));
            }
            if buffer.len() > MAX_RESPONSE_BYTES {
                return Err(error::response("SMTP response too large"));
            }
            pre = buffer.len();

            #[cfg(feature = "tracing")]
            tracing::debug!("<< {}", escape_crlf(&buffer));
            match parse_response(&buffer) {
                Ok((_remaining, response)) => {
                    return if response.is_positive() {
                        Ok(response)
                    } else {
                        Err(error::code(
                            response.code(),
                            Some(response.message().collect()),
                        ))
                    };
                }
                Err(nom::Err::Failure(e)) => {
                    return Err(error::response(e.to_string()));
                }
                Err(nom::Err::Incomplete(_)) => { /* read more */ }
                Err(nom::Err::Error(e)) => {
                    return Err(error::response(e.to_string()));
                }
            }
        }

        Err(error::response("incomplete response"))
    }

    /// The X509 certificate of the server (DER encoded)
    #[cfg(any(feature = "native-tls", feature = "rustls", feature = "boring-tls"))]
    #[cfg_attr(
        docsrs,
        doc(cfg(any(feature = "native-tls", feature = "rustls", feature = "boring-tls")))
    )]
    pub fn peer_certificate(&self) -> Result<Vec<u8>, Error> {
        self.stream.get_ref().peer_certificate()
    }

    /// Currently this is only avaialable when using Boring TLS and
    /// returns the result of the verification of the TLS certificate
    /// presented by the peer, if any. Only the last error encountered
    /// during verification is presented.
    /// It can be useful when you don't want to fail outright the TLS
    /// negotiation, for example when a self-signed certificate is
    /// encountered, but still want to record metrics or log the fact.
    /// When using DANE verification, the PKI root of trust moves from
    /// the CAs to DNS, so self-signed certificates are permitted as long
    /// as the TLSA records match the leaf or issuer certificates.
    /// It cannot be called on non Boring TLS streams.
    #[cfg(feature = "boring-tls")]
    #[cfg_attr(docsrs, doc(cfg(feature = "boring-tls")))]
    pub fn tls_verify_result(&self) -> Result<(), Error> {
        self.stream.get_ref().tls_verify_result()
    }

    /// All the X509 certificates of the chain (DER encoded)
    #[cfg(any(feature = "rustls", feature = "boring-tls"))]
    #[cfg_attr(docsrs, doc(cfg(any(feature = "rustls", feature = "boring-tls"))))]
    pub fn certificate_chain(&self) -> Result<Vec<Vec<u8>>, Error> {
        self.stream.get_ref().certificate_chain()
    }
}

#[cfg(all(test, feature = "tokio1"))]
mod phase_timeout_tests {
    use std::{
        fmt,
        future::Future,
        io,
        net::{Ipv4Addr, SocketAddr},
        pin::Pin,
        task::{Context, Poll},
        time::Duration,
    };

    #[cfg(feature = "tokio1-rustls")]
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    #[cfg(feature = "tokio1-rustls")]
    use std::sync::Arc;
    use tokio1_crate::io::{
        AsyncBufReadExt as _, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf,
    };

    use super::*;

    #[cfg(feature = "tokio1-rustls")]
    const TEST_CA_DER_BASE64: &str = "MIIBoDCCAUWgAwIBAgIUQltW8tjmRLr4QjHNjnIXOGd4oqcwCgYIKoZIzj0EAwIwHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMB4XDTI2MDczMDE2MDE1NFoXDTM2MDcyNzE2MDE1NFowHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEowlFdKeMeRMDaJroLiqhOMAQ1dKYMuoX/SXdgSSY0fIcL7K4mv7z8Xqg5iLrw84NQxGZt36GLxNaGfSLmCR6nqNjMGEwHQYDVR0OBBYEFKzJ36GY9x0+2bor86BZVX+U3mOLMB8GA1UdIwQYMBaAFKzJ36GY9x0+2bor86BZVX+U3mOLMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgKEMAoGCCqGSM49BAMCA0kAMEYCIQD5sjoNPPW9m+gCspyKyj9AOdgwZiavQhgDeIvu5hzVgQIhALJpuju+3/idyBTJ1qGomBG4aRuIO9cHhLwuMtAVtMvt";
    #[cfg(feature = "tokio1-rustls")]
    const TEST_CERT_DER_BASE64: &str = "MIIBxjCCAWygAwIBAgIUM95kwE13FWtKVQEkqO3f8DeMFAgwCgYIKoZIzj0EAwIwHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMB4XDTI2MDczMDE2MDE1NFoXDTM2MDcyNzE2MDE1NFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAElFRQ2c4qt7037iUsrzdKiDrS/euRkQ3z5uCpfrYsFVhe3g4ffc5IBLZDWSEUP0EJvyEOOg5KL1by1ZGYC/d+S6OBkjCBjzAMBgNVHRMBAf8EAjAAMA4GA1UdDwEB/wQEAwIHgDATBgNVHSUEDDAKBggrBgEFBQcDATAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwHQYDVR0OBBYEFM3UTgErLg6p5+pv13yFNths9Lb9MB8GA1UdIwQYMBaAFKzJ36GY9x0+2bor86BZVX+U3mOLMAoGCCqGSM49BAMCA0gAMEUCIEmgwMiWttP7OvYXRkvPm/5c64vpxLLtT+Jg6E4g+OnYAiEAp742aGep2AEwIRP9YXI8RLjhaseLGUQT7R4AWFwnYZE=";
    #[cfg(feature = "tokio1-rustls")]
    const TEST_KEY_DER_BASE64: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg8ZClgJ8kAl4AVnA0D9d0PXx2siCJiOmjud/vD1NKSqehRANCAASUVFDZziq3vTfuJSyvN0qIOtL965GRDfPm4Kl+tiwVWF7eDh99zkgEtkNZIRQ/QQm/IQ46DkovVvLVkZgL935L";

    struct TestStream(tokio1_crate::io::DuplexStream);

    impl fmt::Debug for TestStream {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("TestStream")
        }
    }

    impl AsyncTokioStream for TestStream {
        fn peer_addr(&self) -> io::Result<SocketAddr> {
            Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, 2525)))
        }
    }

    impl AsyncRead for TestStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for TestStream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, io::Error>> {
            Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Pin::new(&mut self.get_mut().0).poll_flush(cx)
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), io::Error>> {
            Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
        }
    }

    #[derive(Debug)]
    struct NeverReadyStream;

    impl AsyncTokioStream for NeverReadyStream {
        fn peer_addr(&self) -> io::Result<SocketAddr> {
            Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, 2525)))
        }
    }

    impl AsyncRead for NeverReadyStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for NeverReadyStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<Result<usize, io::Error>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Pending
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Debug)]
    struct ZeroWriteStream;

    impl AsyncTokioStream for ZeroWriteStream {
        fn peer_addr(&self) -> io::Result<SocketAddr> {
            Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, 2525)))
        }
    }

    impl AsyncRead for ZeroWriteStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for ZeroWriteStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<Result<usize, io::Error>> {
            Poll::Ready(Ok(0))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn connection_from_stream(stream: Box<dyn AsyncTokioStream>) -> AsyncSmtpConnection {
        #[allow(deprecated)]
        let stream = AsyncNetworkStream::use_existing_tokio1(stream);
        AsyncSmtpConnection {
            stream: BufReader::new(stream),
            panic: false,
            server_info: ServerInfo::default(),
        }
    }

    fn connection(stream: tokio1_crate::io::DuplexStream) -> AsyncSmtpConnection {
        connection_from_stream(Box::new(TestStream(stream)))
    }

    fn run_paused(future: impl Future<Output = ()>) {
        tokio1_crate::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(future);
    }

    fn run_loopback(future: impl Future<Output = ()>) {
        tokio1_crate::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future);
    }

    fn setup_timeouts() -> AsyncSmtpConnectTimeouts {
        AsyncSmtpConnectTimeouts {
            connect: Duration::from_secs(60),
            tls_handshake: Duration::from_secs(60),
            greeting: Duration::from_secs(300),
            ehlo: Duration::from_secs(300),
        }
    }

    async fn read_command<R>(connection: &mut tokio1_crate::io::BufReader<R>) -> String
    where
        R: AsyncRead + Unpin,
    {
        let mut line = String::new();
        connection.read_line(&mut line).await.unwrap();
        line
    }

    #[test]
    fn greeting_ehlo_and_each_auth_challenge_receive_fresh_budgets() {
        run_paused(async {
            let (client, server) = tokio1_crate::io::duplex(256);
            let server_task = tokio1_crate::spawn(async move {
                let mut connection = tokio1_crate::io::BufReader::new(server);

                tokio1_crate::time::sleep(Duration::from_secs(240)).await;
                connection
                    .get_mut()
                    .write_all(b"220 fictional SMTP ready\r\n")
                    .await
                    .unwrap();
                assert!(read_command(&mut connection).await.starts_with("EHLO "));

                tokio1_crate::time::sleep(Duration::from_secs(240)).await;
                connection
                    .get_mut()
                    .write_all(b"250-fictional\r\n250 AUTH LOGIN\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_command(&mut connection).await, "AUTH LOGIN\r\n");

                tokio1_crate::time::sleep(Duration::from_secs(240)).await;
                connection
                    .get_mut()
                    .write_all(b"334 VXNlcm5hbWU6\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_command(&mut connection).await, "dXNlcg==\r\n");

                tokio1_crate::time::sleep(Duration::from_secs(240)).await;
                connection
                    .get_mut()
                    .write_all(b"334 UGFzc3dvcmQ6\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_command(&mut connection).await, "cGFzcw==\r\n");

                tokio1_crate::time::sleep(Duration::from_secs(240)).await;
                connection
                    .get_mut()
                    .write_all(b"235 2.7.0 authenticated\r\n")
                    .await
                    .unwrap();
            });

            #[allow(deprecated)]
            let client = AsyncNetworkStream::use_existing_tokio1(Box::new(TestStream(client)));
            let mut connection = AsyncSmtpConnection::connect_impl_with_timeouts(
                client,
                &ClientId::Domain("client.example".to_owned()),
                setup_timeouts(),
            )
            .await
            .unwrap();
            let response = connection
                .auth_with_timeout(
                    &[Mechanism::Login],
                    &Credentials::new("user".to_owned(), "pass".to_owned()),
                    Duration::from_secs(300),
                )
                .await
                .unwrap();

            assert!(response.has_code(235));
            server_task.await.unwrap();
        });
    }

    #[test]
    fn greeting_timeout_is_bounded() {
        run_paused(async {
            let (client, _server) = tokio1_crate::io::duplex(64);
            #[allow(deprecated)]
            let client = AsyncNetworkStream::use_existing_tokio1(Box::new(TestStream(client)));
            let mut timeouts = setup_timeouts();
            timeouts.greeting = Duration::from_secs(10);

            let error = match AsyncSmtpConnection::connect_impl_with_timeouts(
                client,
                &ClientId::Domain("client.example".to_owned()),
                timeouts,
            )
            .await
            {
                Ok(_) => panic!("connection unexpectedly succeeded without an SMTP greeting"),
                Err(error) => error,
            };

            assert!(error.is_timeout());
            assert!(error.to_string().contains("server greeting"));
        });
    }

    #[test]
    #[allow(deprecated)]
    fn dns_tcp_connection_timeout_is_bounded() {
        run_paused(async {
            let error = connect_network_with_timeout(
                Duration::from_secs(10),
                std::future::pending::<Result<AsyncNetworkStream, Error>>(),
            )
            .await
            .unwrap_err();

            assert!(error.is_timeout());
            assert!(error.to_string().contains("DNS/TCP connection"));
        });
    }

    #[test]
    fn public_constructor_reaches_the_greeting_timeout_after_tcp_connects() {
        run_loopback(async {
            let listener = tokio1_crate::net::TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let (accepted_tx, accepted_rx) = tokio1_crate::sync::oneshot::channel();
            let server_task = tokio1_crate::spawn(async move {
                let (_stream, _) = listener.accept().await.unwrap();
                accepted_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            });
            let mut timeouts = setup_timeouts();
            timeouts.connect = Duration::from_secs(2);
            timeouts.greeting = Duration::from_millis(50);
            let client_id = ClientId::Domain("client.example".to_owned());
            let connect = AsyncSmtpConnection::connect_tokio1_with_timeouts(
                address, timeouts, &client_id, None, None,
            );
            tokio1_crate::pin!(connect);
            let result = tokio1_crate::select! {
                result = &mut connect => result,
                accepted = accepted_rx => {
                    accepted.unwrap();
                    connect.await
                }
            };
            let error = match result {
                Ok(_) => panic!("connection unexpectedly succeeded without an SMTP greeting"),
                Err(error) => error,
            };

            assert!(error.is_timeout());
            assert!(error.to_string().contains("server greeting"));
            server_task.abort();
        });
    }

    #[test]
    fn intermediate_greeting_and_ehlo_replies_do_not_advance_setup() {
        run_paused(async {
            for wrong_stage in ["greeting", "EHLO"] {
                let (client, mut server) = tokio1_crate::io::duplex(256);
                let server_task = tokio1_crate::spawn(async move {
                    if wrong_stage == "greeting" {
                        server
                            .write_all(b"354 wrong-stage greeting\r\n")
                            .await
                            .unwrap();
                    } else {
                        server.write_all(b"220 ready\r\n").await.unwrap();
                        let mut connection = tokio1_crate::io::BufReader::new(server);
                        assert!(read_command(&mut connection).await.starts_with("EHLO "));
                        connection
                            .get_mut()
                            .write_all(b"354 wrong-stage EHLO reply\r\n")
                            .await
                            .unwrap();
                        return;
                    }
                });

                #[allow(deprecated)]
                let client = AsyncNetworkStream::use_existing_tokio1(Box::new(TestStream(client)));
                let result = AsyncSmtpConnection::connect_impl_with_timeouts(
                    client,
                    &ClientId::Domain("client.example".to_owned()),
                    setup_timeouts(),
                )
                .await;
                let error = match result {
                    Ok(_) => panic!("{wrong_stage} intermediate reply advanced SMTP setup"),
                    Err(error) => error,
                };
                assert!(error.is_response());
                server_task.await.unwrap();
            }
        });
    }

    #[test]
    fn intermediate_auth_reply_is_not_authentication_success() {
        run_paused(async {
            for final_reply in [
                &b"354 wrong-stage AUTH reply\r\n"[..],
                &b"250 wrong-code AUTH completion\r\n"[..],
            ] {
                let (client, server) = tokio1_crate::io::duplex(256);
                let server_task = tokio1_crate::spawn(async move {
                    let mut connection = tokio1_crate::io::BufReader::new(server);
                    connection
                        .get_mut()
                        .write_all(b"220 ready\r\n")
                        .await
                        .unwrap();
                    assert!(read_command(&mut connection).await.starts_with("EHLO "));
                    connection
                        .get_mut()
                        .write_all(b"250-fictional\r\n250 AUTH LOGIN\r\n")
                        .await
                        .unwrap();
                    assert_eq!(read_command(&mut connection).await, "AUTH LOGIN\r\n");
                    connection.get_mut().write_all(final_reply).await.unwrap();
                });

                #[allow(deprecated)]
                let client = AsyncNetworkStream::use_existing_tokio1(Box::new(TestStream(client)));
                let mut connection = AsyncSmtpConnection::connect_impl_with_timeouts(
                    client,
                    &ClientId::Domain("client.example".to_owned()),
                    setup_timeouts(),
                )
                .await
                .unwrap();
                let error = connection
                    .auth_with_timeout(
                        &[Mechanism::Login],
                        &Credentials::new("user".to_owned(), "pass".to_owned()),
                        Duration::from_secs(300),
                    )
                    .await
                    .unwrap_err();
                assert!(error.is_response());
                assert!(connection.has_broken());
                server_task.await.unwrap();
            }
        });
    }

    #[test]
    fn malformed_auth_challenge_poisoned_the_connection_without_cleanup_commands() {
        run_paused(async {
            let (client, server) = tokio1_crate::io::duplex(256);
            let server_task = tokio1_crate::spawn(async move {
                let mut connection = tokio1_crate::io::BufReader::new(server);
                connection
                    .get_mut()
                    .write_all(b"220 ready\r\n")
                    .await
                    .unwrap();
                assert!(read_command(&mut connection).await.starts_with("EHLO "));
                connection
                    .get_mut()
                    .write_all(b"250-fictional\r\n250 AUTH LOGIN\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_command(&mut connection).await, "AUTH LOGIN\r\n");
                connection
                    .get_mut()
                    .write_all(b"334 !!!not-base64!!!\r\n")
                    .await
                    .unwrap();

                let mut forbidden = String::new();
                assert!(
                    tokio1_crate::time::timeout(
                        Duration::from_secs(1),
                        connection.read_line(&mut forbidden)
                    )
                    .await
                    .is_err(),
                    "client sent a command after malformed AUTH challenge: {forbidden:?}"
                );
            });

            #[allow(deprecated)]
            let client = AsyncNetworkStream::use_existing_tokio1(Box::new(TestStream(client)));
            let mut connection = AsyncSmtpConnection::connect_impl_with_timeouts(
                client,
                &ClientId::Domain("client.example".to_owned()),
                setup_timeouts(),
            )
            .await
            .unwrap();
            let error = connection
                .auth_with_timeout(
                    &[Mechanism::Login],
                    &Credentials::new("user".to_owned(), "pass".to_owned()),
                    Duration::from_secs(300),
                )
                .await
                .unwrap_err();

            assert!(error.is_response());
            assert!(connection.has_broken());
            server_task.await.unwrap();
        });
    }

    #[test]
    fn auth_challenge_limit_poisoned_the_connection_without_quit() {
        run_paused(async {
            let (client, server) = tokio1_crate::io::duplex(512);
            let server_task = tokio1_crate::spawn(async move {
                let mut connection = tokio1_crate::io::BufReader::new(server);
                connection
                    .get_mut()
                    .write_all(b"220 ready\r\n")
                    .await
                    .unwrap();
                assert!(read_command(&mut connection).await.starts_with("EHLO "));
                connection
                    .get_mut()
                    .write_all(b"250-fictional\r\n250 AUTH LOGIN\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_command(&mut connection).await, "AUTH LOGIN\r\n");
                for challenge in 0..10 {
                    connection
                        .get_mut()
                        .write_all(b"334 VXNlcm5hbWU6\r\n")
                        .await
                        .unwrap();
                    assert!(
                        !read_command(&mut connection).await.is_empty(),
                        "client omitted response to AUTH challenge {challenge}"
                    );
                }
                connection
                    .get_mut()
                    .write_all(b"334 VXNlcm5hbWU6\r\n")
                    .await
                    .unwrap();
                let mut forbidden = String::new();
                assert!(
                    tokio1_crate::time::timeout(
                        Duration::from_secs(1),
                        connection.read_line(&mut forbidden)
                    )
                    .await
                    .is_err(),
                    "client exceeded AUTH challenge limit or sent QUIT: {forbidden:?}"
                );
            });

            #[allow(deprecated)]
            let client = AsyncNetworkStream::use_existing_tokio1(Box::new(TestStream(client)));
            let mut connection = AsyncSmtpConnection::connect_impl_with_timeouts(
                client,
                &ClientId::Domain("client.example".to_owned()),
                setup_timeouts(),
            )
            .await
            .unwrap();
            let error = connection
                .auth_with_timeout(
                    &[Mechanism::Login],
                    &Credentials::new("user".to_owned(), "pass".to_owned()),
                    Duration::from_secs(300),
                )
                .await
                .unwrap_err();

            assert!(error.is_response());
            assert!(connection.has_broken());
            server_task.await.unwrap();
        });
    }

    #[cfg(feature = "tokio1-rustls")]
    fn test_tls_parameters() -> TlsParameters {
        use crate::transport::smtp::client::Certificate;

        TlsParameters::builder("127.0.0.1".to_owned())
            .add_root_certificate(
                Certificate::from_der(STANDARD.decode(TEST_CA_DER_BASE64).unwrap()).unwrap(),
            )
            .build()
            .unwrap()
    }

    #[cfg(feature = "tokio1-rustls")]
    fn test_tls_acceptor() -> tokio1_rustls::TlsAcceptor {
        use tokio1_rustls::rustls::{
            ServerConfig,
            pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
        };

        let certificate = CertificateDer::from(STANDARD.decode(TEST_CERT_DER_BASE64).unwrap());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            STANDARD.decode(TEST_KEY_DER_BASE64).unwrap(),
        ));
        tokio1_rustls::TlsAcceptor::from(Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![certificate], key)
                .unwrap(),
        ))
    }

    #[cfg(feature = "tokio1-rustls")]
    #[test]
    fn implicit_tls_handshake_timeout_is_bounded() {
        run_paused(async {
            #[allow(deprecated)]
            let mut stream = AsyncNetworkStream::use_existing_tokio1(Box::new(NeverReadyStream));
            let error = upgrade_implicit_tls_with_timeout(
                &mut stream,
                test_tls_parameters(),
                Duration::from_secs(10),
            )
            .await
            .unwrap_err();

            assert!(error.is_timeout());
            assert!(error.to_string().contains("implicit TLS handshake"));
        });
    }

    #[cfg(feature = "tokio1-rustls")]
    #[test]
    fn starttls_requires_exact_220_before_entering_tls() {
        run_paused(async {
            let (client, server) = tokio1_crate::io::duplex(256);
            let server_task = tokio1_crate::spawn(async move {
                let mut connection = tokio1_crate::io::BufReader::new(server);
                connection
                    .get_mut()
                    .write_all(b"220 ready\r\n")
                    .await
                    .unwrap();
                assert!(read_command(&mut connection).await.starts_with("EHLO "));
                connection
                    .get_mut()
                    .write_all(b"250-fictional\r\n250 STARTTLS\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_command(&mut connection).await, "STARTTLS\r\n");
                connection
                    .get_mut()
                    .write_all(b"250 wrong-stage STARTTLS reply\r\n")
                    .await
                    .unwrap();

                let mut forbidden_tls_bytes = [0; 1];
                assert!(
                    tokio1_crate::time::timeout(
                        Duration::from_secs(1),
                        connection.read(&mut forbidden_tls_bytes)
                    )
                    .await
                    .is_err(),
                    "client entered a TLS handshake after a non-220 STARTTLS reply"
                );
            });

            #[allow(deprecated)]
            let client = AsyncNetworkStream::use_existing_tokio1(Box::new(TestStream(client)));
            let mut connection = AsyncSmtpConnection::connect_impl_with_timeouts(
                client,
                &ClientId::Domain("client.example".to_owned()),
                setup_timeouts(),
            )
            .await
            .unwrap();
            let error = connection
                .starttls_with_timeouts(
                    test_tls_parameters(),
                    &ClientId::Domain("client.example".to_owned()),
                    AsyncSmtpStartTlsTimeouts {
                        command: Duration::from_secs(300),
                        tls_handshake: Duration::from_secs(60),
                        ehlo: Duration::from_secs(300),
                    },
                )
                .await
                .unwrap_err();

            assert!(error.is_response());
            assert!(connection.has_broken());
            server_task.await.unwrap();
        });
    }

    #[cfg(feature = "tokio1-rustls")]
    #[test]
    fn starttls_handshake_stall_uses_its_own_timeout() {
        run_paused(async {
            let (client, server) = tokio1_crate::io::duplex(256);
            let server_task = tokio1_crate::spawn(async move {
                let mut connection = tokio1_crate::io::BufReader::new(server);
                connection
                    .get_mut()
                    .write_all(b"220 ready\r\n")
                    .await
                    .unwrap();
                assert!(read_command(&mut connection).await.starts_with("EHLO "));
                connection
                    .get_mut()
                    .write_all(b"250-fictional\r\n250 STARTTLS\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_command(&mut connection).await, "STARTTLS\r\n");
                connection
                    .get_mut()
                    .write_all(b"220 begin TLS\r\n")
                    .await
                    .unwrap();
                std::future::pending::<()>().await;
            });

            #[allow(deprecated)]
            let client = AsyncNetworkStream::use_existing_tokio1(Box::new(TestStream(client)));
            let mut connection = AsyncSmtpConnection::connect_impl_with_timeouts(
                client,
                &ClientId::Domain("client.example".to_owned()),
                setup_timeouts(),
            )
            .await
            .unwrap();
            let error = connection
                .starttls_with_timeouts(
                    test_tls_parameters(),
                    &ClientId::Domain("client.example".to_owned()),
                    AsyncSmtpStartTlsTimeouts {
                        command: Duration::from_secs(300),
                        tls_handshake: Duration::from_secs(60),
                        ehlo: Duration::from_secs(300),
                    },
                )
                .await
                .unwrap_err();

            assert!(error.is_timeout());
            assert!(error.to_string().contains("STARTTLS handshake"));
            assert!(connection.has_broken());
            server_task.abort();
        });
    }

    #[cfg(feature = "tokio1-rustls")]
    #[test]
    fn post_starttls_ehlo_stall_uses_its_own_timeout() {
        run_loopback(async {
            let listener = tokio1_crate::net::TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let acceptor = test_tls_acceptor();
            let server_task = tokio1_crate::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut connection = tokio1_crate::io::BufReader::new(stream);
                connection
                    .get_mut()
                    .write_all(b"220 ready\r\n")
                    .await
                    .unwrap();
                assert!(read_command(&mut connection).await.starts_with("EHLO "));
                connection
                    .get_mut()
                    .write_all(b"250-fictional\r\n250 STARTTLS\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_command(&mut connection).await, "STARTTLS\r\n");
                connection
                    .get_mut()
                    .write_all(b"220 begin TLS\r\n")
                    .await
                    .unwrap();

                let stream = acceptor.accept(connection.into_inner()).await.unwrap();
                let mut connection = tokio1_crate::io::BufReader::new(stream);
                assert!(read_command(&mut connection).await.starts_with("EHLO "));
                std::future::pending::<()>().await;
            });

            let mut connection = AsyncSmtpConnection::connect_tokio1_with_timeouts(
                address,
                setup_timeouts(),
                &ClientId::Domain("client.example".to_owned()),
                None,
                None,
            )
            .await
            .unwrap();
            let error = connection
                .starttls_with_timeouts(
                    test_tls_parameters(),
                    &ClientId::Domain("client.example".to_owned()),
                    AsyncSmtpStartTlsTimeouts {
                        command: Duration::from_secs(2),
                        tls_handshake: Duration::from_secs(2),
                        ehlo: Duration::from_millis(50),
                    },
                )
                .await
                .unwrap_err();

            assert!(error.is_timeout());
            assert!(error.to_string().contains("EHLO command"));
            assert!(connection.has_broken());
            server_task.abort();
        });
    }

    #[cfg(feature = "tokio1-rustls")]
    #[test]
    fn starttls_uses_separate_command_handshake_and_post_tls_ehlo_phases() {
        run_loopback(async {
            let listener = tokio1_crate::net::TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let acceptor = test_tls_acceptor();
            let server_task = tokio1_crate::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut connection = tokio1_crate::io::BufReader::new(stream);
                connection
                    .get_mut()
                    .write_all(b"220 ready\r\n")
                    .await
                    .unwrap();
                assert!(read_command(&mut connection).await.starts_with("EHLO "));
                connection
                    .get_mut()
                    .write_all(b"250-fictional\r\n250 STARTTLS\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_command(&mut connection).await, "STARTTLS\r\n");
                connection
                    .get_mut()
                    .write_all(b"220 begin TLS\r\n")
                    .await
                    .unwrap();

                let stream = acceptor.accept(connection.into_inner()).await.unwrap();
                let mut connection = tokio1_crate::io::BufReader::new(stream);
                assert!(read_command(&mut connection).await.starts_with("EHLO "));
                connection
                    .get_mut()
                    .write_all(b"250-fictional\r\n250 AUTH PLAIN\r\n")
                    .await
                    .unwrap();
                assert!(
                    read_command(&mut connection)
                        .await
                        .starts_with("AUTH PLAIN ")
                );
                connection
                    .get_mut()
                    .write_all(b"235 2.7.0 authenticated\r\n")
                    .await
                    .unwrap();
            });

            let mut connection = AsyncSmtpConnection::connect_tokio1_with_timeouts(
                address,
                setup_timeouts(),
                &ClientId::Domain("client.example".to_owned()),
                None,
                None,
            )
            .await
            .unwrap();
            assert!(!connection.is_encrypted());
            assert!(connection.can_starttls());
            assert!(
                connection
                    .auth_with_timeout(
                        &[Mechanism::Plain],
                        &Credentials::new("user".to_owned(), "pass".to_owned()),
                        Duration::from_secs(300),
                    )
                    .await
                    .is_err(),
                "AUTH was available before the mandatory TLS upgrade"
            );
            assert!(!connection.has_broken());

            connection
                .starttls_with_timeouts(
                    test_tls_parameters(),
                    &ClientId::Domain("client.example".to_owned()),
                    AsyncSmtpStartTlsTimeouts {
                        command: Duration::from_secs(300),
                        tls_handshake: Duration::from_secs(60),
                        ehlo: Duration::from_secs(300),
                    },
                )
                .await
                .unwrap();
            assert!(connection.is_encrypted());
            assert!(
                connection
                    .auth_with_timeout(
                        &[Mechanism::Plain],
                        &Credentials::new("user".to_owned(), "pass".to_owned()),
                        Duration::from_secs(300),
                    )
                    .await
                    .unwrap()
                    .has_code(235)
            );
            server_task.await.unwrap();
        });
    }

    #[test]
    fn phase_aware_data_preserves_transparency_and_waits_for_final_reply() {
        run_paused(async {
            let (client, mut server) = tokio1_crate::io::duplex(256);
            let expected = b"..one\r\n..two\r\n\r\n.\r\n";
            let server_task = tokio1_crate::spawn(async move {
                let mut received = vec![0; expected.len()];
                server.read_exact(&mut received).await.unwrap();
                assert_eq!(received, expected);
                server.write_all(b"250 2.0.0 accepted\r\n").await.unwrap();
            });

            let response = connection(client)
                .message_with_timeouts(
                    b".one\r\n.two\r\n",
                    2,
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .await
                .unwrap();

            assert!(response.is_positive());
            server_task.await.unwrap();
        });
    }

    async fn capture_until_terminator(mut server: tokio1_crate::io::DuplexStream) -> Vec<u8> {
        let mut wire = Vec::new();
        loop {
            wire.push(server.read_u8().await.unwrap());
            if wire.ends_with(b"\r\n.\r\n") {
                server.write_all(b"250 2.0.0 accepted\r\n").await.unwrap();
                return wire;
            }
        }
    }

    #[test]
    fn every_content_chunk_boundary_matches_the_existing_lettre_codec() {
        run_paused(async {
            let message = b".leading\rbare carriage\n.bare newline\r\n..crlf dot\r\nend";

            let (legacy_client, legacy_server) = tokio1_crate::io::duplex(512);
            let legacy_capture = tokio1_crate::spawn(capture_until_terminator(legacy_server));
            connection(legacy_client).message(message).await.unwrap();
            let expected = legacy_capture.await.unwrap();

            for chunk_size in 1..=message.len() {
                let (client, server) = tokio1_crate::io::duplex(512);
                let capture = tokio1_crate::spawn(capture_until_terminator(server));
                connection(client)
                    .message_with_timeouts(
                        message,
                        chunk_size,
                        Duration::from_secs(1),
                        Duration::from_secs(1),
                        Duration::from_secs(1),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    capture.await.unwrap(),
                    expected,
                    "chunk size {chunk_size} changed SMTP DATA encoding"
                );
            }
        });
    }

    #[test]
    fn blocked_content_write_is_known_to_precede_the_terminator() {
        run_paused(async {
            let mut connection = connection_from_stream(Box::new(NeverReadyStream));
            let error = connection
                .message_with_timeouts(
                    b"content that cannot write even one byte",
                    64,
                    Duration::from_secs(180),
                    Duration::from_secs(180),
                    Duration::from_secs(600),
                )
                .await
                .unwrap_err();

            assert_eq!(error.phase(), AsyncSmtpDataPhase::Content);
            assert!(error.is_timeout());
            assert!(connection.has_broken());
        });
    }

    #[test]
    fn zero_length_content_and_terminator_writes_fail_without_spinning() {
        run_paused(async {
            for (message, expected_phase) in [
                (&b"content"[..], AsyncSmtpDataPhase::Content),
                (&b""[..], AsyncSmtpDataPhase::Terminator),
            ] {
                let mut connection = connection_from_stream(Box::new(ZeroWriteStream));
                let error = connection
                    .message_with_timeouts(
                        message,
                        64,
                        Duration::from_secs(180),
                        Duration::from_secs(180),
                        Duration::from_secs(600),
                    )
                    .await
                    .unwrap_err();

                assert_eq!(error.phase(), expected_phase);
                let smtp_error = error.smtp_error().unwrap();
                assert!(
                    smtp_error
                        .to_string()
                        .contains("failed to make progress writing SMTP DATA")
                );
                assert!(connection.has_broken());
            }
        });
    }

    #[test]
    fn data_progress_restarts_the_timer_instead_of_using_an_aggregate_deadline() {
        run_paused(async {
            let (client, mut server) = tokio1_crate::io::duplex(1);
            let server_task = tokio1_crate::spawn(async move {
                let expected = b"abc\r\n.\r\n";
                let mut received = Vec::with_capacity(expected.len());
                for _ in expected {
                    tokio1_crate::time::sleep(Duration::from_secs(120)).await;
                    received.push(server.read_u8().await.unwrap());
                }
                assert_eq!(received, expected);
                server.write_all(b"250 2.0.0 accepted\r\n").await.unwrap();
            });

            let response = connection(client)
                .message_with_timeouts(
                    b"abc",
                    3,
                    Duration::from_secs(180),
                    Duration::from_secs(180),
                    Duration::from_secs(600),
                )
                .await
                .unwrap();

            assert!(response.is_positive());
            server_task.await.unwrap();
        });
    }

    #[test]
    fn blocked_terminator_write_is_reported_as_ambiguous() {
        run_paused(async {
            let mut connection = connection_from_stream(Box::new(NeverReadyStream));
            let error = connection
                .message_with_timeouts(
                    b"",
                    64,
                    Duration::from_secs(180),
                    Duration::from_secs(180),
                    Duration::from_secs(600),
                )
                .await
                .unwrap_err();

            assert_eq!(error.phase(), AsyncSmtpDataPhase::Terminator);
            assert!(error.is_timeout());
            assert!(connection.has_broken());
        });
    }

    #[test]
    fn final_reply_stall_is_reported_after_the_terminator() {
        run_paused(async {
            let (client, mut server) = tokio1_crate::io::duplex(64);
            let server_task = tokio1_crate::spawn(async move {
                let mut terminator = [0; 5];
                server.read_exact(&mut terminator).await.unwrap();
                assert_eq!(&terminator, b"\r\n.\r\n");
                std::future::pending::<()>().await;
            });

            let mut connection = connection(client);
            let error = connection
                .message_with_timeouts(
                    b"",
                    64,
                    Duration::from_secs(180),
                    Duration::from_secs(180),
                    Duration::from_secs(600),
                )
                .await
                .unwrap_err();

            assert_eq!(error.phase(), AsyncSmtpDataPhase::FinalResponse);
            assert!(error.is_timeout());
            assert!(connection.has_broken());
            server_task.abort();
        });
    }

    #[test]
    fn explicit_final_rejection_retains_the_smtp_status() {
        run_paused(async {
            let (client, mut server) = tokio1_crate::io::duplex(64);
            let server_task = tokio1_crate::spawn(async move {
                let mut terminator = [0; 5];
                server.read_exact(&mut terminator).await.unwrap();
                server.write_all(b"554 5.6.0 rejected\r\n").await.unwrap();
            });

            let mut connection = connection(client);
            let error = connection
                .message_with_timeouts(
                    b"",
                    64,
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .await
                .unwrap_err();

            assert_eq!(error.phase(), AsyncSmtpDataPhase::FinalResponse);
            assert!(error.smtp_error().unwrap().is_permanent());
            assert!(connection.has_broken());
            server_task.await.unwrap();
        });
    }

    #[test]
    fn explicit_transient_final_rejection_retains_the_smtp_status() {
        run_paused(async {
            let (client, mut server) = tokio1_crate::io::duplex(64);
            let server_task = tokio1_crate::spawn(async move {
                let mut terminator = [0; 5];
                server.read_exact(&mut terminator).await.unwrap();
                assert_eq!(&terminator, b"\r\n.\r\n");
                server.write_all(b"451 4.3.0 try later\r\n").await.unwrap();
            });

            let mut connection = connection(client);
            let error = connection
                .message_with_timeouts(
                    b"",
                    64,
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .await
                .unwrap_err();

            assert_eq!(error.phase(), AsyncSmtpDataPhase::FinalResponse);
            assert!(error.smtp_error().unwrap().is_transient());
            assert!(connection.has_broken());
            server_task.await.unwrap();
        });
    }

    #[test]
    fn eof_and_malformed_final_replies_remain_ambiguous() {
        run_paused(async {
            for final_bytes in [None, Some(&b"not an SMTP response\r\n"[..])] {
                let (client, mut server) = tokio1_crate::io::duplex(64);
                let server_task = tokio1_crate::spawn(async move {
                    let mut terminator = [0; 5];
                    server.read_exact(&mut terminator).await.unwrap();
                    assert_eq!(&terminator, b"\r\n.\r\n");
                    if let Some(final_bytes) = final_bytes {
                        server.write_all(final_bytes).await.unwrap();
                    }
                });

                let mut connection = connection(client);
                let error = connection
                    .message_with_timeouts(
                        b"",
                        64,
                        Duration::from_secs(1),
                        Duration::from_secs(1),
                        Duration::from_secs(1),
                    )
                    .await
                    .unwrap_err();

                assert_eq!(error.phase(), AsyncSmtpDataPhase::FinalResponse);
                let smtp_error = error.smtp_error().unwrap();
                assert!(smtp_error.is_response());
                assert!(!smtp_error.is_transient());
                assert!(!smtp_error.is_permanent());
                assert!(connection.has_broken());
                server_task.await.unwrap();
            }
        });
    }

    #[test]
    fn intermediate_final_reply_is_not_mistaken_for_delivery_acceptance() {
        run_paused(async {
            let (client, mut server) = tokio1_crate::io::duplex(64);
            let server_task = tokio1_crate::spawn(async move {
                let mut terminator = [0; 5];
                server.read_exact(&mut terminator).await.unwrap();
                assert_eq!(&terminator, b"\r\n.\r\n");
                server.write_all(b"354 continue sending\r\n").await.unwrap();
            });

            let mut connection = connection(client);
            let error = connection
                .message_with_timeouts(
                    b"",
                    64,
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .await
                .unwrap_err();

            assert_eq!(error.phase(), AsyncSmtpDataPhase::FinalResponse);
            let smtp_error = error.smtp_error().unwrap();
            assert!(smtp_error.is_response());
            assert!(!smtp_error.is_transient());
            assert!(!smtp_error.is_permanent());
            assert!(connection.has_broken());
            server_task.await.unwrap();
        });
    }
}
