use std::collections::{HashMap, HashSet};

use itertools::Itertools;
use tracing::warn;
use turbopath::AbsoluteSystemPath;
use turborepo_microfrontends::{Config as MFEConfig, Error, MICROFRONTENDS_PACKAGES};
use turborepo_repository::package_graph::{PackageGraph, PackageName};

use crate::{
    config,
    run::task_id::{TaskId, TaskName},
    turbo_json::TurboJson,
};

#[derive(Debug, Clone)]
pub struct MicrofrontendsConfigs {
    configs: HashMap<String, HashSet<TaskId<'static>>>,
    config_filenames: HashMap<String, String>,
    mfe_package: Option<&'static str>,
}

impl MicrofrontendsConfigs {
    /// Constructs a collection of configurations from disk
    pub fn from_disk(
        repo_root: &AbsoluteSystemPath,
        package_graph: &PackageGraph,
    ) -> Result<Option<Self>, Error> {
        Self::from_configs(package_graph.packages().map(|(name, info)| {
            (
                name.as_str(),
                MFEConfig::load_from_dir(&repo_root.resolve(info.package_path())),
            )
        }))
    }

    /// Constructs a collection of configurations from a list of configurations
    pub fn from_configs<'a>(
        configs: impl Iterator<Item = (&'a str, Result<Option<MFEConfig>, Error>)>,
    ) -> Result<Option<Self>, Error> {
        let PackageGraphResult {
            configs,
            config_filenames,
            missing_default_apps,
            unsupported_version,
            mfe_package,
        } = PackageGraphResult::new(configs)?;

        for (package, err) in unsupported_version {
            warn!("Ignoring {package}: {err}");
        }

        if !missing_default_apps.is_empty() {
            warn!(
                "Missing default applications: {}",
                missing_default_apps.join(", ")
            );
        }

        Ok((!configs.is_empty()).then_some(Self {
            configs,
            config_filenames,
            mfe_package,
        }))
    }

    pub fn contains_package(&self, package_name: &str) -> bool {
        self.configs.contains_key(package_name)
    }

    pub fn configs(&self) -> impl Iterator<Item = (&String, &HashSet<TaskId<'static>>)> {
        self.configs.iter()
    }

    pub fn get(&self, package_name: &str) -> Option<&HashSet<TaskId<'static>>> {
        self.configs.get(package_name)
    }

    pub fn task_has_mfe_proxy(&self, task_id: &TaskId) -> bool {
        self.configs
            .values()
            .any(|dev_tasks| dev_tasks.contains(task_id))
    }

    pub fn config_filename(&self, package_name: &str) -> Option<&str> {
        let filename = self.config_filenames.get(package_name)?;
        Some(filename.as_str())
    }

    pub fn update_turbo_json(
        &self,
        package_name: &PackageName,
        turbo_json: Result<TurboJson, config::Error>,
    ) -> Result<TurboJson, config::Error> {
        // If package either
        // - contains the proxy task
        // - a member of one of the microfrontends
        // then we need to modify its task definitions
        if let Some(FindResult { dev, proxy }) = self.package_turbo_json_update(package_name) {
            // We need to modify turbo.json, use default one if there isn't one present
            let mut turbo_json = turbo_json.or_else(|err| match err {
                config::Error::NoTurboJSON => Ok(TurboJson::default()),
                err => Err(err),
            })?;

            // If the current package contains the proxy task, then add that definition
            if proxy.package() == package_name.as_str() {
                turbo_json.with_proxy(self.mfe_package);
            }

            if let Some(dev) = dev {
                // If this package has a dev task that's part of the MFE, then we make sure the
                // proxy gets included in the task graph.
                turbo_json.with_sibling(
                    TaskName::from(dev.task()).into_owned(),
                    &proxy.as_task_name(),
                );
            }

            Ok(turbo_json)
        } else {
            turbo_json
        }
    }

    fn package_turbo_json_update<'a>(
        &'a self,
        package_name: &PackageName,
    ) -> Option<FindResult<'a>> {
        self.configs().find_map(|(config, tasks)| {
            let dev_task = tasks.iter().find_map(|task| {
                (task.package() == package_name.as_str()).then(|| FindResult {
                    dev: Some(task.as_borrowed()),
                    proxy: TaskId::new(config, "proxy"),
                })
            });
            let proxy_owner = (config.as_str() == package_name.as_str()).then(|| FindResult {
                dev: None,
                proxy: TaskId::new(config, "proxy"),
            });
            dev_task.or(proxy_owner)
        })
    }
}

// Internal struct used to capture the results of checking the package graph
struct PackageGraphResult {
    configs: HashMap<String, HashSet<TaskId<'static>>>,
    config_filenames: HashMap<String, String>,
    missing_default_apps: Vec<String>,
    unsupported_version: Vec<(String, String)>,
    mfe_package: Option<&'static str>,
}

impl PackageGraphResult {
    fn new<'a>(
        packages: impl Iterator<Item = (&'a str, Result<Option<MFEConfig>, Error>)>,
    ) -> Result<Self, Error> {
        let mut configs = HashMap::new();
        let mut config_filenames = HashMap::new();
        let mut referenced_default_apps = HashSet::new();
        let mut unsupported_version = Vec::new();
        let mut mfe_package = None;
        // We sort packages to ensure deterministic behavior
        let sorted_packages = packages.sorted_by(|(a, _), (b, _)| a.cmp(b));
        for (package_name, config) in sorted_packages {
            if let Some(pkg) = MICROFRONTENDS_PACKAGES
                .iter()
                .find(|static_pkg| package_name == **static_pkg)
            {
                mfe_package = Some(*pkg);
            }

            let Some(config) = config.or_else(|err| match err {
                turborepo_microfrontends::Error::UnsupportedVersion(_) => {
                    unsupported_version.push((package_name.to_string(), err.to_string()));
                    Ok(None)
                }
                turborepo_microfrontends::Error::ChildConfig { reference } => {
                    referenced_default_apps.insert(reference);
                    Ok(None)
                }
                err => Err(err),
            })?
            else {
                continue;
            };
            let tasks = config
                .development_tasks()
                .map(|(application, options)| {
                    let dev_task = options.unwrap_or("dev");
                    TaskId::new(application, dev_task).into_owned()
                })
                .collect();
            configs.insert(package_name.to_string(), tasks);
            config_filenames.insert(package_name.to_string(), config.filename().to_owned());
        }
        let default_apps_found = configs.keys().cloned().collect();
        let mut missing_default_apps = referenced_default_apps
            .difference(&default_apps_found)
            .cloned()
            .collect::<Vec<_>>();
        missing_default_apps.sort();
        Ok(Self {
            configs,
            config_filenames,
            missing_default_apps,
            unsupported_version,
            mfe_package,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct FindResult<'a> {
    dev: Option<TaskId<'a>>,
    proxy: TaskId<'a>,
}

#[cfg(test)]
mod test {
    use serde_json::json;
    use test_case::test_case;
    use turborepo_microfrontends::{
        MICROFRONTENDS_PACKAGE_EXTERNAL, MICROFRONTENDS_PACKAGE_INTERNAL,
    };

    use super::*;

    macro_rules! mfe_configs {
        {$($config_owner:expr => $dev_tasks:expr),+} => {
            {
                let mut _map = std::collections::HashMap::new();
                $(
                    let mut _dev_tasks = std::collections::HashSet::new();
                    for _dev_task in $dev_tasks.as_slice() {
                        _dev_tasks.insert(crate::run::task_id::TaskName::from(*_dev_task).task_id().unwrap().into_owned());
                    }
                    _map.insert($config_owner.to_string(), _dev_tasks);
                )+
                _map
            }
        };
    }

    struct PackageUpdateTest {
        package_name: &'static str,
        result: Option<TestFindResult>,
    }

    struct TestFindResult {
        dev: Option<&'static str>,
        proxy: &'static str,
    }

    impl PackageUpdateTest {
        pub const fn new(package_name: &'static str) -> Self {
            Self {
                package_name,
                result: None,
            }
        }

        pub const fn dev(mut self, dev: &'static str, proxy: &'static str) -> Self {
            self.result = Some(TestFindResult {
                dev: Some(dev),
                proxy,
            });
            self
        }

        pub const fn proxy_only(mut self, proxy: &'static str) -> Self {
            self.result = Some(TestFindResult { dev: None, proxy });
            self
        }

        pub fn package_name(&self) -> PackageName {
            PackageName::from(self.package_name)
        }

        pub fn expected(&self) -> Option<FindResult> {
            match self.result {
                Some(TestFindResult {
                    dev: Some(dev),
                    proxy,
                }) => Some(FindResult {
                    dev: Some(Self::str_to_task(dev)),
                    proxy: Self::str_to_task(proxy),
                }),
                Some(TestFindResult { dev: None, proxy }) => Some(FindResult {
                    dev: None,
                    proxy: Self::str_to_task(proxy),
                }),
                None => None,
            }
        }

        fn str_to_task(s: &str) -> TaskId<'static> {
            crate::run::task_id::TaskName::from(s)
                .task_id()
                .unwrap()
                .into_owned()
        }
    }

    const NON_MFE_PKG: PackageUpdateTest = PackageUpdateTest::new("other-pkg");
    const MFE_CONFIG_PKG: PackageUpdateTest =
        PackageUpdateTest::new("mfe-config-pkg").proxy_only("mfe-config-pkg#proxy");
    const MFE_CONFIG_PKG_DEV_TASK: PackageUpdateTest =
        PackageUpdateTest::new("web").dev("web#dev", "mfe-config-pkg#proxy");
    const DEFAULT_APP_PROXY: PackageUpdateTest =
        PackageUpdateTest::new("mfe-docs").dev("mfe-docs#serve", "mfe-web#proxy");
    const DEFAULT_APP_PROXY_AND_DEV: PackageUpdateTest =
        PackageUpdateTest::new("mfe-web").dev("mfe-web#dev", "mfe-web#proxy");

    #[test_case(NON_MFE_PKG)]
    #[test_case(MFE_CONFIG_PKG)]
    #[test_case(MFE_CONFIG_PKG_DEV_TASK)]
    #[test_case(DEFAULT_APP_PROXY)]
    #[test_case(DEFAULT_APP_PROXY_AND_DEV)]
    fn test_package_turbo_json_update(test: PackageUpdateTest) {
        let configs = mfe_configs!(
            "mfe-config-pkg" => ["web#dev", "docs#dev"],
            "mfe-web" => ["mfe-web#dev", "mfe-docs#serve"]
        );
        let mfe = MicrofrontendsConfigs {
            configs,
            config_filenames: HashMap::new(),
            mfe_package: None,
        };
        assert_eq!(
            mfe.package_turbo_json_update(&test.package_name()),
            test.expected()
        );
    }

    #[test]
    fn test_mfe_package_is_found() {
        let result = PackageGraphResult::new(
            vec![
                // These should never be present in the same graph, but if for some reason they
                // are, we defer to the external variant.
                (MICROFRONTENDS_PACKAGE_EXTERNAL, Ok(None)),
                (MICROFRONTENDS_PACKAGE_INTERNAL, Ok(None)),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(result.mfe_package, Some(MICROFRONTENDS_PACKAGE_EXTERNAL));
    }

    #[test]
    fn test_no_mfe_package() {
        let result =
            PackageGraphResult::new(vec![("foo", Ok(None)), ("bar", Ok(None))].into_iter())
                .unwrap();
        assert_eq!(result.mfe_package, None);
    }

    #[test]
    fn test_unsupported_versions_ignored() {
        let result = PackageGraphResult::new(
            vec![("foo", Err(Error::UnsupportedVersion("bad version".into())))].into_iter(),
        )
        .unwrap();
        assert_eq!(result.configs, HashMap::new());
    }

    #[test]
    fn test_child_configs_with_missing_default() {
        let result = PackageGraphResult::new(
            vec![(
                "child",
                Err(Error::ChildConfig {
                    reference: "main".into(),
                }),
            )]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(result.configs, HashMap::new());
        assert_eq!(result.missing_default_apps, &["main".to_string()]);
    }

    #[test]
    fn test_io_err_stops_traversal() {
        let result = PackageGraphResult::new(
            vec![
                (
                    "a",
                    Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "something",
                    ))),
                ),
                (
                    "b",
                    Err(Error::ChildConfig {
                        reference: "main".into(),
                    }),
                ),
            ]
            .into_iter(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_dev_task_collection() {
        let config = MFEConfig::from_str(
            &serde_json::to_string_pretty(&json!({
                "version": "2",
                "applications": {
                    "web": {},
                    "docs": {
                        "development": {
                            "task": "serve"
                        }
                    }
                }
            }))
            .unwrap(),
            "something.txt",
        )
        .unwrap();
        let result = PackageGraphResult::new(vec![("web", Ok(Some(config)))].into_iter()).unwrap();
        assert_eq!(
            result.configs,
            mfe_configs!(
                "web" => ["web#dev", "docs#serve"]
            )
        )
    }
}
