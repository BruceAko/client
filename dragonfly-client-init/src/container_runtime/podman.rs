/*
 *     Copyright 2024 The Dragonfly Authors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use dragonfly_client_config::dfinit;
use dragonfly_client_core::{
    error::{ErrorType, OrErr},
    Error, Result,
};
use tokio::{self, fs};
use toml_edit::{value, Array, ArrayOfTables, Item, Table, Value};
use tracing::{info, instrument};
use url::Url;

/// Podman represents the podman runtime manager.
#[derive(Debug, Clone)]
pub struct Podman {
    /// config is the configuration for initializing
    /// runtime environment for the dfdaemon.
    config: dfinit::Podman,

    /// proxy_config is the configuration for the dfdaemon's proxy server.
    proxy_config: dfinit::Proxy,
}

/// Podman implements the podman runtime manager.
impl Podman {
    /// new creates a new podman runtime manager.
    #[instrument(skip_all)]
    pub fn new(config: dfinit::Podman, proxy_config: dfinit::Proxy) -> Self {
        Self {
            config,
            proxy_config,
        }
    }

    /// run runs the podman runtime to initialize
    /// runtime environment for the dfdaemon.
    #[instrument(skip_all)]
    pub async fn run(&self) -> Result<()> {
        let mut registries_config_table = toml_edit::DocumentMut::new();
        registries_config_table.set_implicit(true);

        // Add unqualified-search-registries to registries config.
        let mut unqualified_search_registries = Array::default();
        for unqualified_search_registry in self.config.unqualified_search_registries.clone() {
            unqualified_search_registries.push(Value::from(unqualified_search_registry));
        }
        registries_config_table.insert(
            "unqualified-search-registries",
            value(unqualified_search_registries),
        );

        // Parse proxy address to get host and port.
        let proxy_url =
            Url::parse(self.proxy_config.addr.as_str()).or_err(ErrorType::ParseError)?;
        let proxy_host = proxy_url
            .host_str()
            .ok_or(Error::Unknown("host not found".to_string()))?;
        let proxy_port = proxy_url
            .port_or_known_default()
            .ok_or(Error::Unknown("port not found".to_string()))?;
        let proxy_location = format!("{}:{}", proxy_host, proxy_port);

        // Add registries to the registries config.
        let mut registries_table = ArrayOfTables::new();
        for registry in self.config.registries.clone() {
            info!("add registry: {:?}", registry);
            let mut registry_mirror_table = Table::new();
            registry_mirror_table.set_implicit(true);
            registry_mirror_table.insert("insecure", value(true));
            registry_mirror_table.insert("location", value(proxy_location.as_str()));

            let mut registry_mirrors_table = ArrayOfTables::new();
            registry_mirrors_table.push(registry_mirror_table);

            let mut registry_table = Table::new();
            registry_table.set_implicit(true);
            registry_table.insert("prefix", value(registry.prefix));
            registry_table.insert("location", value(registry.location));
            registry_table.insert("mirror", Item::ArrayOfTables(registry_mirrors_table));

            registries_table.push(registry_table);
        }
        registries_config_table.insert("registry", Item::ArrayOfTables(registries_table));

        let registries_config_dir = self
            .config
            .config_path
            .parent()
            .ok_or(Error::Unknown("invalid config path".to_string()))?;
        fs::create_dir_all(registries_config_dir.as_os_str()).await?;
        fs::write(
            self.config.config_path.as_os_str(),
            registries_config_table.to_string().as_bytes(),
        )
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_podman_config() {
        use tempfile::NamedTempFile;

        let podman_config_file = NamedTempFile::new().unwrap();
        let podman = Podman::new(
            dfinit::Podman {
                config_path: podman_config_file.path().to_path_buf(),
                registries: vec![dfinit::PodmanRegistry {
                    prefix: "registry.example.com".into(),
                    location: "registry.example.com".into(),
                }],
                unqualified_search_registries: vec!["registry.example.com".into()],
            },
            dfinit::Proxy {
                addr: "http://127.0.0.1:5000".into(),
            },
        );
        let result = podman.run().await;

        assert!(result.is_ok());

        // get the contents of the file
        let contents = fs::read_to_string(podman_config_file.path().to_path_buf())
            .await
            .unwrap();
        let expected_contents = r#"unqualified-search-registries = ["registry.example.com"]

[[registry]]
prefix = "registry.example.com"
location = "registry.example.com"

[[registry.mirror]]
insecure = true
location = "127.0.0.1:5000"
"#;
        // assert that the contents of the file are as expected
        assert_eq!(contents, expected_contents);

        // clean up
        fs::remove_file(podman_config_file.path().to_path_buf())
            .await
            .unwrap();
    }
}
