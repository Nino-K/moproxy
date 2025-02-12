use anyhow::{anyhow, bail, Context};
use futures_util::{stream, StreamExt};
use ini::Ini;
use parking_lot::RwLock;
use std::{collections::HashSet, io, net::SocketAddr, net::ToSocketAddrs, path::PathBuf, sync::Arc, time::Duration};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, instrument, warn};

use crate::{cli::CliArgs, FromOptionStr};
#[cfg(feature = "web_console")]
use moproxy::web::WebServer;
use moproxy::{
    client::{FailedClient, NewClient},
    futures_stream::TcpListenerStream,
    monitor::Monitor,
    policy::{parser, ActionType, Policy},
    proxy::{ProxyProto, ProxyServer, UserPassAuthCredential},
    web::WebServerListener,
};

#[derive(Clone)]
pub(crate) struct MoProxy {
    cli_args: Arc<CliArgs>,
    server_list_config: Arc<ServerListConfig>,
    pub(crate) monitor: Monitor,
    direct_server: Arc<ProxyServer>,
    pub(crate) policy: Arc<RwLock<Policy>>,
    #[cfg(feature = "web_console")]
    web_server: Option<WebServer>,
}

pub(crate) struct MoProxyListener {
    moproxy: MoProxy,
    listeners: Vec<TcpListenerStream>,
    #[cfg(feature = "web_console")]
    web_server: Option<WebServerListener>,
}

#[derive(Debug)]
enum PolicyResult {
    Filtered(Vec<Arc<ProxyServer>>),
    Direct,
    Reject,
}

impl MoProxy {
    pub(crate) async fn new(args: CliArgs) -> anyhow::Result<Self> {
        // Load proxy server list
        let server_list_config = ServerListConfig::new(&args);
        let servers = server_list_config.load().context("fail to load servers")?;
        let direct_server = Arc::new(ProxyServer::direct(args.max_wait));

        // Load policy
        let policy = {
            if let Some(ref path) = args.policy {
                let policy = Policy::load_from_file(path).context("cannot to load policy")?;
                Arc::new(RwLock::new(policy))
            } else {
                Default::default()
            }
        };

        // Setup proxy monitor
        let graphite = args.graphite;
        #[cfg(feature = "score_script")]
        let mut monitor = Monitor::new(servers, graphite);
        #[cfg(not(feature = "score_script"))]
        let monitor = Monitor::new(servers, graphite);
        #[cfg(feature = "score_script")]
        {
            if let Some(ref path) = args.score_script {
                monitor
                    .load_score_script(path)
                    .context("fail to load Lua script")?;
            }
        }

        // Setup web console
        #[cfg(feature = "web_console")]
        let web_server = if let Some(addr) = &args.web_bind {
            Some(WebServer::new(monitor.clone(), addr.into())?)
        } else {
            None
        };

        // Launch monitor
        if args.probe_secs > 0 {
            tokio::spawn(monitor.clone().monitor_delay(args.probe_secs));
        }

        Ok(Self {
            cli_args: Arc::new(args),
            server_list_config: Arc::new(server_list_config),
            direct_server,
            monitor,
            policy,
            #[cfg(feature = "web_console")]
            web_server,
        })
    }

    pub(crate) fn reload(&self) -> anyhow::Result<()> {
        // Load proxy server list
        let servers = self.server_list_config.load()?;
        // Load policy
        let policy = match &self.cli_args.policy {
            Some(path) => Policy::load_from_file(path).context("cannot to load policy")?,
            _ => Default::default(),
        };
        // TODO: reload lua script

        // Apply only if no error occur
        self.monitor.update_servers(servers);
        *self.policy.write() = policy;
        Ok(())
    }

    pub(crate) async fn listen(&self) -> anyhow::Result<MoProxyListener> {
        let ports: HashSet<_> = self.cli_args.port.iter().collect();
        let mut listeners = Vec::with_capacity(ports.len());
        for port in ports {
            let addr = SocketAddr::new(self.cli_args.host, *port);
            let listener = TcpListener::bind(&addr)
                .await
                .context("cannot bind to port")?;
            info!("listen on {}", addr);
            #[cfg(target_os = "linux")]
            if let Some(ref alg) = self.cli_args.cong_local {
                use moproxy::linux::tcp::TcpListenerExt;

                info!("set {} on {}", alg, addr);
                listener.set_congestion(alg).expect(
                    "fail to set tcp congestion algorithm. \
                    check tcp_allowed_congestion_control?",
                );
            }
            listeners.push(TcpListenerStream(listener));
        }
        #[cfg(feature = "web_console")]
        let web_server = if let Some(web) = &self.web_server {
            Some(web.listen().await?)
        } else {
            None
        };

        Ok(MoProxyListener {
            moproxy: self.clone(),
            listeners,
            #[cfg(feature = "web_console")]
            web_server,
        })
    }

    fn apply_policy(&self, client: &NewClient) -> PolicyResult {
        let action = self.policy.read().matches(&client.features());
        match action.action {
            ActionType::Reject => PolicyResult::Reject,
            ActionType::Direct => PolicyResult::Direct,
            ActionType::Require(caps) => {
                let servers = self
                    .monitor
                    .servers()
                    .into_iter()
                    .filter(|s| caps.iter().all(|c| s.capable_anyof(c)))
                    .collect();
                PolicyResult::Filtered(servers)
            }
        }
    }

    #[instrument(level = "error", skip_all, fields(on_port=sock.local_addr()?.port(), peer=?sock.peer_addr()?))]
    async fn handle_client(&self, sock: TcpStream) -> io::Result<()> {
        let mut client = NewClient::from_socket(sock).await?;
        let args = &self.cli_args;

        if (args.remote_dns || args.n_parallel > 1) && client.dest.port == 443 {
            // Try parse TLS client hello
            client.retrieve_dest_from_sni().await?;
            if args.remote_dns {
                client.override_dest_with_sni();
            }
        }
        let result = match self.apply_policy(&client) {
            PolicyResult::Reject => {
                info!("rejected by policy");
                return Ok(());
            }
            PolicyResult::Direct => client
                .direct_connect(self.direct_server.clone())
                .await
                .map_err(|err| err.into()),
            PolicyResult::Filtered(proxies) => {
                client.connect_server(proxies, args.n_parallel).await
            }
        };
        let client = match result {
            Ok(client) => client,
            Err(FailedClient::Recoverable(client)) if args.allow_direct => {
                client.direct_connect(self.direct_server.clone()).await?
            }
            Err(_) => return Ok(()),
        };
        client.serve().await
    }
}

impl MoProxyListener {
    pub(crate) async fn handle_forever(mut self) {
        #[cfg(feature = "web_console")]
        if let Some(web) = self.web_server {
            web.run_background()
        }

        let mut clients = stream::select_all(self.listeners.iter_mut());
        while let Some(sock) = clients.next().await {
            let moproxy = self.moproxy.clone();
            match sock {
                Ok(sock) => {
                    tokio::spawn(async move {
                        if let Err(e) = moproxy.handle_client(sock).await {
                            info!("error on hanle client: {}", e);
                        }
                    });
                }
                Err(err) => info!("error on accept client: {}", err),
            }
        }
    }
}

struct ServerListConfig {
    default_test_dns: SocketAddr,
    default_max_wait: Duration,
    cli_servers: Vec<Arc<ProxyServer>>,
    path: Option<PathBuf>,
    allow_direct: bool,
}

impl ServerListConfig {
    fn new(args: &CliArgs) -> Self {
        let default_test_dns = args.test_dns;
        let default_max_wait = args.max_wait;

        let mut cli_servers = vec![];
        for addr in &args.socks5_servers {
            cli_servers.push(Arc::new(ProxyServer::new(
                *addr,
                ProxyProto::socks5(false),
                default_test_dns,
                default_max_wait,
                None,
                None,
                None,
            )));
        }

        for addr in &args.http_servers {
            cli_servers.push(Arc::new(ProxyServer::new(
                *addr,
                ProxyProto::http(false, None),
                default_test_dns,
                default_max_wait,
                None,
                None,
                None,
            )));
        }

        let path = args.server_list.clone();
        Self {
            default_test_dns,
            default_max_wait,
            cli_servers,
            path,
            allow_direct: args.allow_direct,
        }
    }

    #[instrument(skip_all)]
    fn load(&self) -> anyhow::Result<Vec<Arc<ProxyServer>>> {
        let mut servers = self.cli_servers.clone();
        if let Some(path) = &self.path {
            let ini = Ini::load_from_file(path).context("cannot read server list file")?;
            for (tag, props) in ini.iter() {
                let tag = props.get("tag").or(tag);
                let addr: SocketAddr = props
                    .get("address")
                    .ok_or(anyhow!("address not specified"))?
                    .to_socket_addrs()
                    .context("not a valid socket address")?
                    .next().unwrap();
                let base = props
                    .get("score base")
                    .parse()
                    .context("score base not a integer")?;
                let test_dns = props
                    .get("test dns")
                    .parse()
                    .context("not a valid socket address")?
                    .unwrap_or(self.default_test_dns);
                let max_wait = props
                    .get("max wait")
                    .parse()
                    .context("not a valid number")?
                    .map(Duration::from_secs)
                    .unwrap_or(self.default_max_wait);
                if props.get("listen ports").is_some() {
                    // TODO: add a link to how-to --policy
                    error!("`listen ports` is not longer supported, use --policy instead");
                }
                let (_, capabilities) =
                    parser::capabilities(props.get("capabilities").unwrap_or_default())
                        .map_err(|e| e.to_owned())
                        .context("not a valid list of capabilities")?;
                let proto = match props
                    .get("protocol")
                    .context("protocol not specified")?
                    .to_lowercase()
                    .as_str()
                {
                    "socks5" | "socksv5" => {
                        let fake_hs = props
                            .get("socks fake handshaking")
                            .parse()
                            .context("not a boolean value")?
                            .unwrap_or(false);
                        let username = props.get("socks username").unwrap_or("");
                        let password = props.get("socks password").unwrap_or("");
                        match (username.len(), password.len()) {
                            (0, 0) => ProxyProto::socks5(fake_hs),
                            (0, _) | (_, 0) => bail!("socks username/password is empty"),
                            (u, p) if u > 255 || p > 255 => {
                                bail!("socks username/password too long")
                            }
                            _ => ProxyProto::socks5_with_auth(UserPassAuthCredential::new(
                                username, password,
                            )),
                        }
                    }
                    "http" => {
                        let cwp = props
                            .get("http allow connect payload")
                            .parse()
                            .context("not a boolean value")?
                            .unwrap_or(false);
                        let credential =
                            match (props.get("http username"), props.get("http password")) {
                                (None, None) => None,
                                (Some(user), _) if user.contains(':') => {
                                    bail!("semicolon (:) in http username")
                                }
                                (user, pass) => Some(UserPassAuthCredential::new(
                                    user.unwrap_or(""),
                                    pass.unwrap_or(""),
                                )),
                            };
                        ProxyProto::http(cwp, credential)
                    }
                    _ => bail!("unknown proxy protocol"),
                };
                let server = ProxyServer::new(
                    addr,
                    proto,
                    test_dns,
                    max_wait,
                    Some(capabilities),
                    tag,
                    base,
                );
                servers.push(Arc::new(server));
            }
        }
        if servers.is_empty() && !self.allow_direct {
            bail!("missing server list");
        }
        info!("total {} server(s) loaded", servers.len());
        Ok(servers)
    }
}
