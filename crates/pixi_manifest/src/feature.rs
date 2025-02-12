use std::{
    borrow::{Borrow, Cow},
    collections::HashMap,
    fmt,
    hash::{Hash, Hasher},
};

use indexmap::{IndexMap, IndexSet};
use itertools::Either;
use pixi_spec::PixiSpec;
use rattler_conda_types::{PackageName, Platform};
use rattler_solve::ChannelPriority;
use serde::{de::Error, Deserialize, Deserializer};
use serde_with::{serde_as, SerializeDisplay};

use crate::{
    channel::{PrioritizedChannel, TomlPrioritizedChannelStrOrMap},
    consts,
    parsed_manifest::deserialize_opt_package_map,
    parsed_manifest::deserialize_package_map,
    pypi::{pypi_options::PypiOptions, PyPiPackageName},
    target::Targets,
    task::{Task, TaskName},
    utils::PixiSpanned,
    Activation, PyPiRequirement, SpecType, SystemRequirements, Target, TargetSelector,
};

/// The name of a feature. This is either a string or default for the default
/// feature.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord, SerializeDisplay, Default)]
pub enum FeatureName {
    #[default]
    Default,
    Named(String),
}

impl Hash for FeatureName {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}

impl<'de> Deserialize<'de> for FeatureName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            consts::DEFAULT_FEATURE_NAME => Err(D::Error::custom(
                "The name 'default' is reserved for the default feature",
            )),
            name => Ok(FeatureName::Named(name.to_string())),
        }
    }
}

impl<'s> From<&'s str> for FeatureName {
    fn from(value: &'s str) -> Self {
        match value {
            consts::DEFAULT_FEATURE_NAME => FeatureName::Default,
            name => FeatureName::Named(name.to_string()),
        }
    }
}
impl FeatureName {
    /// Returns the name of the feature or `None` if this is the default
    /// feature.
    pub fn name(&self) -> Option<&str> {
        match self {
            FeatureName::Default => None,
            FeatureName::Named(name) => Some(name),
        }
    }

    pub fn as_str(&self) -> &str {
        self.name().unwrap_or(consts::DEFAULT_FEATURE_NAME)
    }

    /// Returns true if the feature is the default feature.
    pub fn is_default(&self) -> bool {
        matches!(self, FeatureName::Default)
    }
}

impl Borrow<str> for FeatureName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl From<FeatureName> for String {
    fn from(name: FeatureName) -> Self {
        match name {
            FeatureName::Default => consts::DEFAULT_FEATURE_NAME.to_string(),
            FeatureName::Named(name) => name,
        }
    }
}
impl<'a> From<&'a FeatureName> for String {
    fn from(name: &'a FeatureName) -> Self {
        match name {
            FeatureName::Default => consts::DEFAULT_FEATURE_NAME.to_string(),
            FeatureName::Named(name) => name.clone(),
        }
    }
}
impl fmt::Display for FeatureName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeatureName::Default => write!(f, "{}", consts::DEFAULT_FEATURE_NAME),
            FeatureName::Named(name) => write!(f, "{}", name),
        }
    }
}

/// A feature describes a set of functionalities. It allows us to group
/// functionality and its dependencies together.
///
/// Individual features cannot be used directly, instead they are grouped
/// together into environments. Environments are then locked and installed.
#[derive(Debug, Clone)]
pub struct Feature {
    /// The name of the feature or `None` if the feature is the default feature.
    pub name: FeatureName,

    /// The platforms this feature is available on.
    ///
    /// This value is `None` if this feature does not specify any platforms and
    /// the default platforms from the project should be used.
    pub platforms: Option<PixiSpanned<IndexSet<Platform>>>,

    /// Channels specific to this feature.
    ///
    /// This value is `None` if this feature does not specify any channels and
    /// the default channels from the project should be used.
    pub channels: Option<IndexSet<PrioritizedChannel>>,

    /// Channel priority for the solver, if not set the default is used.
    /// This value is `None` and there are multiple features,
    /// it will be seen as unset and overwritten by a set one.
    pub channel_priority: Option<ChannelPriority>,

    /// Additional system requirements
    pub system_requirements: SystemRequirements,

    /// Pypi-related options
    pub pypi_options: Option<PypiOptions>,

    /// Target specific configuration.
    pub targets: Targets,
}

impl Feature {
    /// Construct a new feature with the given name.
    pub fn new(name: FeatureName) -> Self {
        Feature {
            name,
            platforms: None,
            channels: None,
            channel_priority: None,
            system_requirements: SystemRequirements::default(),
            pypi_options: None,

            targets: <Targets as Default>::default(),
        }
    }

    /// Returns true if this feature is the default feature.
    pub fn is_default(&self) -> bool {
        self.name == FeatureName::Default
    }

    /// Returns a mutable reference to the platforms of the feature. Create them
    /// if needed
    pub fn platforms_mut(&mut self) -> &mut IndexSet<Platform> {
        self.platforms
            .get_or_insert_with(Default::default)
            .get_mut()
    }

    /// Returns a mutable reference to the channels of the feature. Create them
    /// if needed
    pub fn channels_mut(&mut self) -> &mut IndexSet<PrioritizedChannel> {
        self.channels.get_or_insert_with(Default::default)
    }

    /// Returns the dependencies of the feature for a given `spec_type` and
    /// `platform`.
    ///
    /// This function returns a [`Cow`]. If the dependencies are not combined or
    /// overwritten by multiple targets than this function returns a
    /// reference to the internal dependencies.
    ///
    /// Returns `None` if this feature does not define any target that has any
    /// of the requested dependencies.
    pub fn dependencies(
        &self,
        spec_type: Option<SpecType>,
        platform: Option<Platform>,
    ) -> Option<Cow<'_, IndexMap<PackageName, PixiSpec>>> {
        self.targets
            .resolve(platform)
            // Get the targets in reverse order, from least specific to most specific.
            // This is required because the extend function will overwrite existing keys.
            .rev()
            .filter_map(|t| t.dependencies(spec_type))
            .filter(|deps| !deps.is_empty())
            .fold(None, |acc, deps| match acc {
                None => Some(deps),
                Some(mut acc) => {
                    let deps_iter = match deps {
                        Cow::Borrowed(deps) => Either::Left(
                            deps.iter().map(|(name, spec)| (name.clone(), spec.clone())),
                        ),
                        Cow::Owned(deps) => Either::Right(deps.into_iter()),
                    };

                    acc.to_mut().extend(deps_iter);
                    Some(acc)
                }
            })
    }

    /// Returns the PyPi dependencies of the feature for a given `platform`.
    ///
    /// This function returns a [`Cow`]. If the dependencies are not combined or
    /// overwritten by multiple targets than this function returns a
    /// reference to the internal dependencies.
    ///
    /// Returns `None` if this feature does not define any target that has any
    /// of the requested dependencies.
    pub fn pypi_dependencies(
        &self,
        platform: Option<Platform>,
    ) -> Option<Cow<'_, IndexMap<PyPiPackageName, PyPiRequirement>>> {
        self.targets
            .resolve(platform)
            // Get the targets in reverse order, from least specific to most specific.
            // This is required because the extend function will overwrite existing keys.
            .rev()
            .filter_map(|t| t.pypi_dependencies.as_ref())
            .filter(|deps| !deps.is_empty())
            .fold(None, |acc, deps| match acc {
                None => Some(Cow::Borrowed(deps)),
                Some(mut acc) => {
                    acc.to_mut().extend(
                        deps.into_iter()
                            .map(|(name, spec)| (name.clone(), spec.clone())),
                    );
                    Some(acc)
                }
            })
    }

    /// Returns the activation scripts for the most specific target that matches
    /// the given `platform`.
    ///
    /// Returns `None` if this feature does not define any target with an
    /// activation.
    pub fn activation_scripts(&self, platform: Option<Platform>) -> Option<&Vec<String>> {
        self.targets
            .resolve(platform)
            .filter_map(|t| t.activation.as_ref())
            .filter_map(|a| a.scripts.as_ref())
            .next()
    }

    /// Returns the activation environment for the most specific target that
    /// matches the given `platform`.
    ///
    /// Returns `None` if this feature does not define any target with an
    /// activation.
    pub fn activation_env(&self, platform: Option<Platform>) -> IndexMap<String, String> {
        self.targets
            .resolve(platform)
            .filter_map(|t| t.activation.as_ref())
            .filter_map(|a| a.env.as_ref())
            .fold(IndexMap::new(), |mut acc, x| {
                for (k, v) in x {
                    if !acc.contains_key(k) {
                        acc.insert(k.clone(), v.clone());
                    }
                }
                acc
            })
    }

    /// Returns true if the feature contains any reference to a pypi
    /// dependencies.
    pub fn has_pypi_dependencies(&self) -> bool {
        self.targets
            .targets()
            .any(|t| t.pypi_dependencies.iter().flatten().next().is_some())
    }

    /// Returns any pypi_options if they are set.
    pub fn pypi_options(&self) -> Option<&PypiOptions> {
        self.pypi_options.as_ref()
    }
}

impl<'de> Deserialize<'de> for Feature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[serde_as]
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "kebab-case")]
        struct FeatureInner {
            #[serde(default)]
            platforms: Option<PixiSpanned<IndexSet<Platform>>>,
            #[serde(default)]
            channels: Option<Vec<TomlPrioritizedChannelStrOrMap>>,
            #[serde(default)]
            channel_priority: Option<ChannelPriority>,
            #[serde(default)]
            system_requirements: SystemRequirements,
            #[serde(default)]
            target: IndexMap<PixiSpanned<TargetSelector>, Target>,

            #[serde(default, deserialize_with = "deserialize_package_map")]
            dependencies: IndexMap<PackageName, PixiSpec>,

            #[serde(default, deserialize_with = "deserialize_opt_package_map")]
            host_dependencies: Option<IndexMap<PackageName, PixiSpec>>,

            #[serde(default, deserialize_with = "deserialize_opt_package_map")]
            build_dependencies: Option<IndexMap<PackageName, PixiSpec>>,

            #[serde(default)]
            pypi_dependencies: Option<IndexMap<PyPiPackageName, PyPiRequirement>>,

            /// Additional information to activate an environment.
            #[serde(default)]
            activation: Option<Activation>,

            /// Target specific tasks to run in the environment
            #[serde(default)]
            tasks: HashMap<TaskName, Task>,

            /// Additional options for PyPi dependencies.
            #[serde(default)]
            pypi_options: Option<PypiOptions>,
        }

        let inner = FeatureInner::deserialize(deserializer)?;
        let mut dependencies = HashMap::from_iter([(SpecType::Run, inner.dependencies)]);
        if let Some(host_deps) = inner.host_dependencies {
            dependencies.insert(SpecType::Host, host_deps);
        }
        if let Some(build_deps) = inner.build_dependencies {
            dependencies.insert(SpecType::Build, build_deps);
        }

        let default_target = Target {
            dependencies,
            pypi_dependencies: inner.pypi_dependencies,
            activation: inner.activation,
            tasks: inner.tasks,
        };

        Ok(Feature {
            name: FeatureName::Default,
            platforms: inner.platforms,
            channels: inner.channels.map(|channels| {
                channels
                    .into_iter()
                    .map(|channel| channel.into_prioritized_channel())
                    .collect()
            }),
            channel_priority: inner.channel_priority,
            system_requirements: inner.system_requirements,
            pypi_options: inner.pypi_options,
            targets: Targets::from_default_and_user_defined(default_target, inner.target),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use assert_matches::assert_matches;

    use super::*;
    use crate::manifests::manifest::Manifest;

    #[test]
    fn test_dependencies_borrowed() {
        let manifest = Manifest::from_str(
            Path::new("pixi.toml"),
            r#"
        [project]
        name = "foo"
        platforms = ["linux-64", "osx-64", "win-64"]
        channels = []

        [dependencies]
        foo = "1.0"

        [host-dependencies]
        foo = "2.0"

        [feature.bla.dependencies]
        foo = "2.0"

        [feature.bla.host-dependencies]
        # empty on purpose
        "#,
        )
        .unwrap();

        assert_matches!(
            manifest
                .default_feature()
                .dependencies(Some(SpecType::Host), None)
                .unwrap(),
            Cow::Borrowed(_),
            "[host-dependencies] should be borrowed"
        );

        assert_matches!(
            manifest
                .default_feature()
                .dependencies(Some(SpecType::Run), None)
                .unwrap(),
            Cow::Borrowed(_),
            "[dependencies] should be borrowed"
        );

        assert_matches!(
            manifest.default_feature().dependencies(None, None).unwrap(),
            Cow::Owned(_),
            "combined dependencies should be owned"
        );

        let bla_feature = manifest
            .parsed
            .features
            .get(&FeatureName::Named(String::from("bla")))
            .unwrap();
        assert_matches!(
            bla_feature.dependencies(Some(SpecType::Run), None).unwrap(),
            Cow::Borrowed(_),
            "[feature.bla.dependencies] should be borrowed"
        );

        assert_matches!(
            bla_feature.dependencies(None, None).unwrap(),
            Cow::Borrowed(_),
            "[feature.bla] combined dependencies should also be borrowed"
        );
    }

    #[test]
    fn test_activation() {
        let manifest = Manifest::from_str(
            Path::new("pixi.toml"),
            r#"
        [project]
        name = "foo"
        platforms = ["linux-64", "osx-64", "win-64"]
        channels = []

        [activation]
        scripts = ["run.bat"]

        [target.linux-64.activation]
        scripts = ["linux-64.bat"]
        "#,
        )
        .unwrap();

        assert_eq!(
            manifest.default_feature().activation_scripts(None).unwrap(),
            &vec!["run.bat".to_string()],
            "should have selected the activation from the [activation] section"
        );
        assert_eq!(
            manifest
                .default_feature()
                .activation_scripts(Some(Platform::Linux64))
                .unwrap(),
            &vec!["linux-64.bat".to_string()],
            "should have selected the activation from the [linux-64] section"
        );
    }

    #[test]
    pub fn test_pypi_options_manifest() {
        let manifest = Manifest::from_str(
            Path::new("pixi.toml"),
            r#"
        [project]
        name = "foo"
        platforms = ["linux-64", "osx-64", "win-64"]
        channels = []

        [project.pypi-options]
        index-url = "https://pypi.org/simple"

        [pypi-options]
        extra-index-urls = ["https://mypypi.org/simple"]
        "#,
        )
        .unwrap();

        // This behavior has changed from >0.22.0
        // and should now be none, previously this was added
        // to the default feature
        assert!(manifest.default_feature().pypi_options().is_some());
        assert!(manifest.parsed.project.pypi_options.is_some());
    }
}
