use crate::auth::{self, Provider};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub(crate) const SCORED_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];
const CLAUDE_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const CODEX_EFFORTS: [&str; 5] = ["minimal", "low", "medium", "high", "xhigh"];
const CLAUDE_ROLLING_ALIASES: [&str; 6] =
    ["default", "best", "opus", "sonnet", "haiku", "opusplan"];
const ROLLING_SUFFIX: &str = "-latest";
const BENCHMARK_VARIANT_SUFFIXES: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];
const MAX_MODEL_LENGTH: usize = 64;
pub(crate) const PINS_FILE: &str = "native-pins-v1.json";
pub(crate) const PINS_SCHEMA: &str = "agent-command-native-pins-v1";
pub(crate) const DEFAULT_SETTING: &str = "AA default setting";

pub(crate) fn native_efforts(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::Claude => &CLAUDE_EFFORTS,
        Provider::Codex => &CODEX_EFFORTS,
    }
}

pub(crate) fn validate_effort(provider: Provider, effort: &str) -> Result<(), String> {
    if native_efforts(provider).contains(&effort) {
        return Ok(());
    }
    Err(format!(
        "{} accepts effort {}; {effort} is not a native option",
        provider.binary(),
        native_efforts(provider).join(", ")
    ))
}

fn model_characters_valid(model: &str, extra: &[u8]) -> bool {
    !model.is_empty()
        && model.len() <= MAX_MODEL_LENGTH
        && model
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && model.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || byte == b'-'
                || extra.contains(&byte)
        })
}

fn has_version_segment(model: &str) -> bool {
    model
        .split('-')
        .any(|segment| !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_digit()))
}

fn benchmark_variant_effort(model: &str) -> Option<(&str, &str)> {
    let (base, suffix) = model.rsplit_once('-')?;
    BENCHMARK_VARIANT_SUFFIXES
        .contains(&suffix)
        .then_some((base, suffix))
}

fn refuse_benchmark_variant(model: &str) -> Result<(), String> {
    match benchmark_variant_effort(model) {
        Some((base, effort)) => Err(format!(
            "{model} names a benchmark effort variant, not a native model; pin {base} with effort {effort}"
        )),
        None => Ok(()),
    }
}

pub(crate) fn validate_pinned_model(provider: Provider, model: &str) -> Result<(), String> {
    refuse_benchmark_variant(model)?;
    match provider {
        Provider::Claude if CLAUDE_ROLLING_ALIASES.contains(&model) => Err(format!(
            "{model} is a rolling alias; pin a full Claude model name such as claude-<family>-<version>"
        )),
        Provider::Claude if !model.starts_with("claude-") || model.ends_with(ROLLING_SUFFIX) => {
            Err(format!("{model} is not a full Claude model name"))
        }
        Provider::Claude if !model_characters_valid(model, b"") => {
            Err(format!("{model} has characters outside a-z, 0-9 and -"))
        }
        Provider::Claude if !has_version_segment(model) => Err(format!(
            "{model} names a model family without a version; pin a full Claude model name"
        )),
        Provider::Codex if model == "default" || model.ends_with(ROLLING_SUFFIX) => {
            Err(format!("{model} is not an exact Codex model ID"))
        }
        Provider::Codex if !model_characters_valid(model, b".") => {
            Err(format!("{model} has characters outside a-z, 0-9, . and -"))
        }
        _ => Ok(()),
    }
}

pub(crate) fn benchmark_slug(provider: Provider, model: &str) -> Option<String> {
    validate_pinned_model(provider, model).ok()?;
    Some(match provider {
        Provider::Claude => model.to_owned(),
        Provider::Codex => model.replace('.', "-"),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Benchmark {
    pub(crate) provider: Provider,
    pub(crate) slug: String,
    pub(crate) effort: Option<&'static str>,
}

impl Benchmark {
    pub(crate) fn models_free_slug(&self) -> String {
        match self.effort {
            Some(effort) => format!("{}-{effort}", self.slug),
            None => self.slug.clone(),
        }
    }
}

pub(crate) fn benchmark(
    provider: Provider,
    model: Option<&str>,
    effort: Option<&str>,
) -> Option<Benchmark> {
    let slug = benchmark_slug(provider, model?)?;
    let effort = match effort {
        None => None,
        Some(effort) => Some(*SCORED_EFFORTS.iter().find(|scored| **scored == effort)?),
    };
    Some(Benchmark {
        provider,
        slug,
        effort,
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pin {
    pub(crate) profile: String,
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
}

impl Pin {
    pub(crate) fn provider(&self) -> Option<Provider> {
        Provider::named(self.profile.split_once('/')?.0)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        let provider = self
            .provider()
            .ok_or_else(|| format!("{} does not name a provider profile", self.profile))?;
        validate_pinned_model(provider, &self.model)?;
        if let Some(effort) = &self.effort {
            validate_effort(provider, effort)?;
        }
        Ok(())
    }

    pub(crate) fn label(&self) -> String {
        match &self.effort {
            Some(effort) => format!("{} {effort}", self.model),
            None => format!("{} {DEFAULT_SETTING}", self.model),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinsRecord {
    schema: String,
    pins: Vec<Pin>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Pins(Vec<Pin>);

fn invalid_state(reason: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

impl Pins {
    pub(crate) fn from_pins(pins: Vec<Pin>) -> Result<Self, String> {
        let mut checked = Self::default();
        for pin in pins {
            pin.validate()?;
            if checked.get(&pin.profile).is_some() {
                return Err(format!("{} is pinned twice", pin.profile));
            }
            checked.0.push(pin);
        }
        Ok(checked)
    }

    pub(crate) fn load(state: &Path) -> io::Result<Self> {
        let bytes = match fs::read(state.join(PINS_FILE)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let record: PinsRecord = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if record.schema != PINS_SCHEMA {
            return Err(invalid_state(format!(
                "schema {} is not {PINS_SCHEMA}",
                record.schema
            )));
        }
        Self::from_pins(record.pins).map_err(invalid_state)
    }

    pub(crate) fn save(&self, state: &Path) -> io::Result<()> {
        let path = state.join(PINS_FILE);
        let temporary = path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
        let mut file = fs::File::create_new(&temporary)?;
        serde_json::to_writer(
            &mut file,
            &PinsRecord {
                schema: PINS_SCHEMA.into(),
                pins: self.0.clone(),
            },
        )?;
        file.sync_all()?;
        drop(file);
        fs::rename(temporary, path)
    }

    pub(crate) fn get(&self, profile: &str) -> Option<&Pin> {
        self.0.iter().find(|pin| pin.profile == profile)
    }

    pub(crate) fn set(&mut self, pin: Pin) -> Result<(), String> {
        pin.validate()?;
        self.remove(&pin.profile);
        self.0.push(pin);
        self.0
            .sort_by(|left, right| left.profile.cmp(&right.profile));
        Ok(())
    }

    pub(crate) fn remove(&mut self, profile: &str) -> bool {
        let before = self.0.len();
        self.0.retain(|pin| pin.profile != profile);
        before != self.0.len()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Pin> {
        self.0.iter()
    }
}

pub(crate) fn load_existing(root: &Path) -> io::Result<Pins> {
    match auth::existing_private_state(root, crate::routing::STATE_DIRECTORY)? {
        Some(state) => Pins::load(&state),
        None => Ok(Pins::default()),
    }
}

fn usage_error(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn known_profile(root: &Path, provider: &str, name: &str) -> io::Result<String> {
    let label = format!("{provider}/{name}");
    if auth::profiles(root)?
        .iter()
        .any(|profile| profile.to_string() == label)
    {
        Ok(label)
    } else {
        Err(usage_error(format!(
            "{label} is not a profile; run agent-command auth login {provider} {name} first"
        )))
    }
}

fn state(root: &Path) -> io::Result<PathBuf> {
    crate::routing::state_directory(root)
}

pub(crate) fn run(args: &[String]) -> io::Result<()> {
    let root = auth::root()?;
    let text: Vec<&str> = args.iter().map(String::as_str).collect();
    match text.as_slice() {
        ["show"] => {
            for pin in load_existing(&root)?.iter() {
                println!("{} · {}", pin.profile, pin.label());
            }
            Ok(())
        }
        ["pin", provider, name, model, rest @ ..] if rest.len() <= 1 => {
            let profile = known_profile(&root, provider, name)?;
            let state = state(&root)?;
            let mut pins = Pins::load(&state)?;
            let pin = Pin {
                profile,
                model: (*model).into(),
                effort: rest.first().map(|effort| (*effort).to_owned()),
            };
            let label = format!("{} · {}", pin.profile, pin.label());
            pins.set(pin).map_err(usage_error)?;
            pins.save(&state)?;
            println!("Pinned {label}");
            Ok(())
        }
        ["unpin", provider, name] => {
            let label = format!("{provider}/{name}");
            let state = state(&root)?;
            let mut pins = Pins::load(&state)?;
            if pins.remove(&label) {
                pins.save(&state)?;
                println!("Unpinned {label}");
            } else {
                println!("{label} has no model pin");
            }
            Ok(())
        }
        _ => {
            println!("agent-command model {{show|pin PROVIDER PROFILE MODEL [EFFORT]|unpin PROVIDER PROFILE}}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(provider: Provider, model: &str, effort: Option<&str>) -> Option<(String, String)> {
        benchmark(provider, Some(model), effort)
            .map(|benchmark| (benchmark.slug.clone(), benchmark.models_free_slug()))
    }

    #[test]
    fn exact_identities_derive_from_native_options_without_a_model_table() {
        assert_eq!(
            identity(Provider::Codex, "gpt-6.2-sol", Some("high")),
            Some(("gpt-6-2-sol".into(), "gpt-6-2-sol-high".into()))
        );
        assert_eq!(
            identity(Provider::Codex, "gpt-6.2-sol", None),
            Some(("gpt-6-2-sol".into(), "gpt-6-2-sol".into()))
        );
        assert_eq!(
            identity(Provider::Claude, "claude-opus-5-5", Some("xhigh")),
            Some(("claude-opus-5-5".into(), "claude-opus-5-5-xhigh".into()))
        );
        assert_eq!(
            identity(Provider::Claude, "claude-opus-5-5", None),
            Some(("claude-opus-5-5".into(), "claude-opus-5-5".into()))
        );
        for effort in ["low", "medium", "high", "xhigh"] {
            assert!(identity(Provider::Codex, "gpt-reserve", Some(effort)).is_some());
        }
        for (provider, model, effort) in [
            (Provider::Claude, "claude-opus-5-5", Some("max")),
            (Provider::Codex, "gpt-6.2-sol", Some("minimal")),
            (Provider::Codex, "gpt-6.2-sol", Some("ultra")),
            (Provider::Claude, "opus", None),
            (Provider::Claude, "sonnet", Some("high")),
            (Provider::Claude, "claude-opus-latest", None),
            (Provider::Claude, "claude-opus", None),
            (Provider::Claude, "claude-opus-5.5", None),
            (Provider::Claude, "gpt-6.2-sol", None),
            (Provider::Codex, "default", None),
        ] {
            assert_eq!(
                identity(provider, model, effort),
                None,
                "{model} {effort:?}"
            );
        }
        assert_eq!(benchmark(Provider::Codex, None, Some("high")), None);
    }

    #[test]
    fn effort_suffixed_names_are_benchmark_variants_not_native_models() {
        for (provider, variant) in [
            (Provider::Claude, "claude-opus-5-5-high"),
            (Provider::Claude, "claude-opus-5-5-xhigh"),
            (Provider::Claude, "claude-opus-5-5-max"),
            (Provider::Claude, "claude-sonnet-5-low"),
            (Provider::Codex, "gpt-6.2-sol-high"),
            (Provider::Codex, "gpt-6-2-sol-medium"),
            (Provider::Codex, "gpt-6.2-sol-minimal"),
        ] {
            let refusal = validate_pinned_model(provider, variant).unwrap_err();
            assert!(
                refusal.contains("benchmark effort variant"),
                "{variant}: {refusal}"
            );
            for effort in [None, Some("high")] {
                assert_eq!(
                    benchmark(provider, Some(variant), effort),
                    None,
                    "{variant}"
                );
            }
        }
        assert_eq!(
            validate_pinned_model(Provider::Claude, "claude-opus-5-5-high").unwrap_err(),
            "claude-opus-5-5-high names a benchmark effort variant, not a native model; pin claude-opus-5-5 with effort high"
        );
        for (provider, model) in [
            (Provider::Claude, "claude-opus-5-5"),
            (Provider::Claude, "claude-opus-6"),
            (Provider::Claude, "claude-sonnet-6-1-20270301"),
            (Provider::Claude, "claude-haiku-6-highland"),
            (Provider::Codex, "gpt-6.2-sol"),
            (Provider::Codex, "gpt-6.2-sol-mini"),
            (Provider::Codex, "gpt-7-nova-maxi"),
        ] {
            assert_eq!(validate_pinned_model(provider, model), Ok(()), "{model}");
            let exact = benchmark(provider, Some(model), Some("high")).unwrap();
            assert_eq!(exact.effort, Some("high"));
            assert_eq!(
                exact.models_free_slug(),
                format!("{}-high", model.replace('.', "-"))
            );
            assert_eq!(benchmark(provider, Some(model), None).unwrap().effort, None);
        }
    }

    #[test]
    fn pins_accept_native_efforts_and_refuse_aliases() {
        let pin = |profile: &str, model: &str, effort: Option<&str>| Pin {
            profile: profile.into(),
            model: model.into(),
            effort: effort.map(str::to_owned),
        };
        let mut pins = Pins::default();
        pins.set(pin("claude/work", "claude-opus-5-5", Some("max")))
            .unwrap();
        pins.set(pin("codex/work", "gpt-6.2-sol", Some("minimal")))
            .unwrap();
        pins.set(pin("claude/work", "claude-opus-5-5", None))
            .unwrap();
        assert_eq!(pins.iter().count(), 2);
        assert_eq!(
            pins.get("claude/work").unwrap().label(),
            "claude-opus-5-5 AA default setting"
        );
        for refused in [
            pin("claude/work", "opus", None),
            pin("claude/work", "claude-sonnet-latest", None),
            pin("claude/work", "claude-opus-5-5", Some("ultra")),
            pin("claude/work", "claude-opus-5-5", Some("minimal")),
            pin("codex/work", "gpt-6.2-sol", Some("max")),
            pin("codex/work", "GPT-6.2", None),
            pin("gemini/work", "claude-opus-5-5", None),
            pin("claude/work", "claude-opus-5-5-high", None),
            pin("codex/work", "gpt-6.2-sol-high", None),
        ] {
            assert!(pins.set(refused.clone()).is_err(), "{refused:?}");
        }
        assert!(Pins::from_pins(vec![
            pin("claude/a", "claude-opus-5-5", None),
            pin("claude/a", "claude-opus-5-5", Some("high")),
        ])
        .is_err());
    }

    #[test]
    fn pinned_models_persist_in_versioned_state() {
        let state = std::env::temp_dir().join(format!("ac-native-pins-{}", std::process::id()));
        let _ = fs::remove_dir_all(&state);
        fs::create_dir_all(&state).unwrap();
        assert_eq!(Pins::load(&state).unwrap(), Pins::default());
        let mut pins = Pins::default();
        pins.set(Pin {
            profile: "claude/work".into(),
            model: "claude-opus-5-5".into(),
            effort: Some("high".into()),
        })
        .unwrap();
        pins.save(&state).unwrap();
        let text = fs::read_to_string(state.join(PINS_FILE)).unwrap();
        assert_eq!(
            text,
            r#"{"schema":"agent-command-native-pins-v1","pins":[{"profile":"claude/work","model":"claude-opus-5-5","effort":"high"}]}"#
        );
        assert_eq!(Pins::load(&state).unwrap(), pins);
        for corrupt in [
            text.replace("native-pins-v1", "native-pins-v0"),
            text.replace("claude-opus-5-5", "opus"),
            text.replace("\"high\"", "\"ultra\""),
            text.replace("\"effort\"", "\"reasoning\""),
            text.replace(
                "\"claude-opus-5-5\",\"effort\":\"high\"",
                "\"claude-opus-5-5-high\",\"effort\":null",
            ),
        ] {
            fs::write(state.join(PINS_FILE), corrupt).unwrap();
            assert!(Pins::load(&state).is_err());
        }
        fs::remove_dir_all(state).unwrap();
    }
}
