use anyhow::{anyhow, Context, Result};
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use client::http::HttpClient;
use futures::{future::Shared, FutureExt, TryFutureExt};
use gpui::{executor::Background, Task};
use parking_lot::Mutex;
use serde::Deserialize;
use smol::{fs, io::BufReader};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct NpmInfo {
    #[serde(default)]
    dist_tags: NpmInfoDistTags,
    versions: Vec<String>,
}

#[derive(Deserialize, Default)]
pub struct NpmInfoDistTags {
    latest: Option<String>,
}

pub struct NodeRuntime {
    http: Arc<dyn HttpClient>,
    background: Arc<Background>,
    installation_path: Mutex<Option<Shared<Task<Result<PathBuf, Arc<anyhow::Error>>>>>>,
}

impl NodeRuntime {
    pub fn new(http: Arc<dyn HttpClient>, background: Arc<Background>) -> Arc<NodeRuntime> {
        Arc::new(NodeRuntime {
            http,
            background,
            installation_path: Mutex::new(None),
        })
    }

    pub async fn binary_path(&self) -> Result<PathBuf> {
        let installation_path = self.install_if_needed().await?;
        Ok(installation_path.join("bin/node"))
    }

    pub async fn npm_package_latest_version(&self, name: &str) -> Result<String> {
        let installation_path = self.install_if_needed().await?;
        let node_binary = installation_path.join("bin/node");
        let npm_file = installation_path.join("bin/npm");

        let output = smol::process::Command::new(node_binary)
            .arg(npm_file)
            .args(["-fetch-retry-mintimeout", "2000"])
            .args(["-fetch-retry-maxtimeout", "5000"])
            .args(["-fetch-timeout", "5000"])
            .args(["info", name, "--json"])
            .output()
            .await
            .context("failed to run npm info")?;
        if !output.status.success() {
            Err(anyhow!(
                "failed to execute npm info:\nstdout: {:?}\nstderr: {:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ))?;
        }
        let mut info: NpmInfo = serde_json::from_slice(&output.stdout)?;
        info.dist_tags
            .latest
            .or_else(|| info.versions.pop())
            .ok_or_else(|| anyhow!("no version found for npm package {}", name))
    }

    pub async fn npm_install_packages(
        &self,
        packages: impl IntoIterator<Item = (&str, &str)>,
        directory: &Path,
    ) -> Result<()> {
        let installation_path = self.install_if_needed().await?;
        let node_binary = installation_path.join("bin/node");
        let npm_file = installation_path.join("bin/npm");

        let output = smol::process::Command::new(node_binary)
            .arg(npm_file)
            .args(["-fetch-retry-mintimeout", "2000"])
            .args(["-fetch-retry-maxtimeout", "5000"])
            .args(["-fetch-timeout", "5000"])
            .arg("install")
            .arg("--prefix")
            .arg(directory)
            .args(
                packages
                    .into_iter()
                    .map(|(name, version)| format!("{name}@{version}")),
            )
            .output()
            .await
            .context("failed to run npm install")?;
        if !output.status.success() {
            Err(anyhow!(
                "failed to execute npm install:\nstdout: {:?}\nstderr: {:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ))?;
        }
        Ok(())
    }

    async fn install_if_needed(&self) -> Result<PathBuf> {
        let task = self
            .installation_path
            .lock()
            .get_or_insert_with(|| {
                let http = self.http.clone();
                self.background
                    .spawn(
                        async move {
                            let version = "v18.15.0";
                            let arch = "arm64";

                            let folder_name = format!("node-{version}-darwin-{arch}");
                            let node_containing_dir = util::paths::SUPPORT_DIR.join("node");
                            let node_dir = node_containing_dir.join(folder_name);
                            let node_binary = node_dir.join("bin/node");

                            if fs::metadata(&node_binary).await.is_err() {
                                _ = fs::remove_dir_all(&node_containing_dir).await;
                                fs::create_dir(&node_containing_dir)
                                    .await
                                    .context("error creating node containing dir")?;

                                let url = format!("https://nodejs.org/dist/{version}/node-{version}-darwin-{arch}.tar.gz");
                                let mut response =
                                    http.get(&url, Default::default(), true)
                                        .await
                                        .context("error downloading Node binary tarball")?;

                                let decompressed_bytes =
                                    GzipDecoder::new(BufReader::new(response.body_mut()));
                                let archive = Archive::new(decompressed_bytes);
                                archive.unpack(&node_containing_dir).await?;
                            }

                            anyhow::Ok(node_dir)
                        }
                        .map_err(Arc::new),
                    )
                    .shared()
            })
            .clone();

        match task.await {
            Ok(path) => Ok(path),
            Err(error) => Err(anyhow!("{}", error)),
        }
    }
}
