//! Wi-Fi: joins the first network from `secrets.toml` that answers, and
//! tries again from the top whenever the connection drops.

use alloc::format;

use embassy_net::{Runner, Stack};
use embassy_time::{Duration, Timer, with_timeout};
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{AuthenticationMethodConfig, Config, Interface, WifiController};
use log::{info, warn};

use crate::config::WifiNetwork;
use crate::config::secrets::WIFI_NETWORKS;
use crate::state;

/// Pause after every network in the list has failed.
const RETRY_DELAY: Duration = Duration::from_secs(10);

/// Give up on joining a network after this long. The driver doesn't always
/// report the outcome of an attempt, and without a limit the frame would
/// wait for it forever.
const JOIN_TIMEOUT: Duration = Duration::from_secs(30);

#[embassy_executor::task]
pub async fn wifi_task(mut controller: WifiController<'static>, stack: Stack<'static>) {
    loop {
        let mut joined = false;
        for network in WIFI_NETWORKS {
            state::set_wifi_status(format!("Connecting to {}...", network.ssid));
            if let Err(e) = join(&mut controller, network).await {
                warn!("Wi-Fi {:?}: {e}", network.ssid);
                continue;
            }
            joined = true;
            info!("Wi-Fi connected to {:?}", network.ssid);
            state::set_wifi_status(format!("Connected to {}, getting an address...", network.ssid));

            // Report the address once DHCP has given one; stay here until
            // the connection drops.
            let connected = async {
                stack.wait_config_up().await;
                if let Some(config) = stack.config_v4() {
                    info!("IP address {}", config.address);
                    state::set_wifi_status(format!("{} ({})", network.ssid, config.address.address()));
                }
                core::future::pending::<()>().await
            };
            embassy_futures::select::select(controller.wait_for_disconnect_async(), connected).await;
            warn!("Wi-Fi {:?} disconnected", network.ssid);
            state::set_wifi_status(format!("Lost {}", network.ssid));
            break;
        }
        if !joined {
            state::set_wifi_status("Can't join any Wi-Fi network in secrets.toml");
            Timer::after(RETRY_DELAY).await;
        }
    }
}

async fn join(
    controller: &mut WifiController<'static>,
    network: &WifiNetwork,
) -> Result<(), &'static str> {
    let ssid = network.ssid.try_into().map_err(|_| "bad SSID")?;
    let authentication = if network.password.is_empty() {
        AuthenticationMethodConfig::Open
    } else {
        AuthenticationMethodConfig::Wpa2Personal(
            network.password.try_into().map_err(|_| "bad password")?,
        )
    };
    let config = Config::Station(
        StationConfig::default()
            .with_ssid(ssid)
            .with_authentication(authentication),
    );
    controller
        .set_config(&config)
        .map_err(|_| "can't configure Wi-Fi")?;
    match with_timeout(JOIN_TIMEOUT, controller.connect_async()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) => Err("connection failed"),
        Err(_) => {
            // Cancel the attempt, so the next one starts cleanly.
            let _ = with_timeout(Duration::from_secs(5), controller.disconnect_async()).await;
            Err("timed out")
        }
    }
}

#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}
