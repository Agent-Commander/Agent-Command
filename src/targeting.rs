use crate::{auth::Provider, routing::Configuration};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Target {
    Provider(Provider),
    Model {
        provider: Provider,
        family: &'static str,
    },
}

const MODEL_FAMILIES: [(&str, Provider); 4] = [
    ("opus", Provider::Claude),
    ("fable", Provider::Claude),
    ("astra", Provider::Codex),
    ("sol", Provider::Codex),
];

const TOKEN_PREFIX: char = '@';

impl Target {
    pub(crate) fn token(self) -> String {
        match self {
            Self::Provider(provider) => format!("{TOKEN_PREFIX}{}", provider.binary()),
            Self::Model { family, .. } => format!("{TOKEN_PREFIX}{family}"),
        }
    }

    pub(crate) fn provider(self) -> Provider {
        match self {
            Self::Provider(provider) | Self::Model { provider, .. } => provider,
        }
    }

    pub(crate) fn matches(self, configuration: &Configuration) -> bool {
        if configuration.provider != self.provider() {
            return false;
        }
        match self {
            Self::Provider(_) => true,
            Self::Model { family, .. } => configuration
                .native_model()
                .is_some_and(|model| model_has_family(model, family)),
        }
    }

    fn named(word: &str) -> Option<Self> {
        let name = word.strip_prefix(TOKEN_PREFIX)?.to_ascii_lowercase();
        if let Some(provider) = Provider::named(&name) {
            return Some(Self::Provider(provider));
        }
        MODEL_FAMILIES
            .into_iter()
            .find(|(family, _)| *family == name)
            .map(|(family, provider)| Self::Model { provider, family })
    }
}

fn model_has_family(model: &str, family: &str) -> bool {
    model
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|segment| segment == family)
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Targeted {
    pub(crate) target: Option<Target>,
    pub(crate) text: String,
}

pub(crate) fn extract(text: &str) -> Result<Targeted, String> {
    let mut target: Option<Target> = None;
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut kept = Vec::new();
        for word in line.split(' ') {
            match Target::named(word) {
                Some(found) => target = Some(merge(target, found)?),
                None => kept.push(word),
            }
        }
        lines.push(collapse_spaces(&kept.join(" ")));
    }
    Ok(Targeted {
        target,
        text: lines.join("\n").trim().to_owned(),
    })
}

fn merge(current: Option<Target>, found: Target) -> Result<Target, String> {
    match current {
        Some(existing) if existing != found => Err(format!(
            "Use one @model target per delegation: found {} and {}.",
            existing.token(),
            found.token()
        )),
        _ => Ok(found),
    }
}

fn collapse_spaces(line: &str) -> String {
    let mut collapsed = String::with_capacity(line.len());
    let mut previous_space = false;
    for c in line.chars() {
        let space = c == ' ';
        if !(space && previous_space) {
            collapsed.push(c);
        }
        previous_space = space;
    }
    collapsed.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::Model;

    fn configuration(provider: Provider, model: Model) -> Configuration {
        Configuration {
            provider,
            model,
            effort: None,
        }
    }

    #[test]
    fn every_inline_token_parses_to_its_target() {
        let cases = [
            ("@claude", Target::Provider(Provider::Claude)),
            ("@codex", Target::Provider(Provider::Codex)),
            ("@gemini", Target::Provider(Provider::Gemini)),
            (
                "@opus",
                Target::Model {
                    provider: Provider::Claude,
                    family: "opus",
                },
            ),
            (
                "@fable",
                Target::Model {
                    provider: Provider::Claude,
                    family: "fable",
                },
            ),
            (
                "@astra",
                Target::Model {
                    provider: Provider::Codex,
                    family: "astra",
                },
            ),
            (
                "@SOL",
                Target::Model {
                    provider: Provider::Codex,
                    family: "sol",
                },
            ),
        ];
        for (token, target) in cases {
            let parsed = extract(&format!("Fix the bug {token} quickly")).unwrap();
            assert_eq!(parsed.target, Some(target), "{token}");
            assert_eq!(parsed.text, "Fix the bug quickly");
            assert_eq!(target.token(), token.to_ascii_lowercase());
        }
    }

    #[test]
    fn untargeted_text_and_unknown_mentions_stay_untouched() {
        let parsed = extract("Bump @types/node and ping @someone\nthen  rerun").unwrap();
        assert_eq!(parsed.target, None);
        assert_eq!(
            parsed.text,
            "Bump @types/node and ping @someone\nthen rerun"
        );
        let parsed = extract("email me@opus.example").unwrap();
        assert_eq!(parsed.target, None);
    }

    #[test]
    fn leading_and_repeated_tokens_are_removed_but_conflicts_are_refused() {
        let parsed = extract("@codex /hard Build it @codex").unwrap();
        assert_eq!(parsed.target, Some(Target::Provider(Provider::Codex)));
        assert_eq!(parsed.text, "/hard Build it");
        assert_eq!(
            extract("@opus build @codex").unwrap_err(),
            "Use one @model target per delegation: found @opus and @codex."
        );
    }

    #[test]
    fn targets_match_provider_and_model_family_segments() {
        let opus_alias = configuration(Provider::Claude, Model::Alias("opus"));
        let fable_pin = configuration(Provider::Claude, Model::Pinned("claude-fable-5-2".into()));
        let sol_pin = configuration(Provider::Codex, Model::Pinned("gpt-6.2-sol".into()));
        let console_pin = configuration(Provider::Codex, Model::Pinned("gpt-console".into()));
        let codex_default = Configuration::provider_default(Provider::Codex);
        let opus = Target::named("@opus").unwrap();
        let fable = Target::named("@fable").unwrap();
        let sol = Target::named("@sol").unwrap();
        let codex = Target::named("@codex").unwrap();
        assert!(opus.matches(&opus_alias));
        assert!(!opus.matches(&fable_pin));
        assert!(fable.matches(&fable_pin));
        assert!(sol.matches(&sol_pin));
        assert!(!sol.matches(&console_pin));
        assert!(!sol.matches(&codex_default));
        assert!(codex.matches(&codex_default));
        assert!(codex.matches(&sol_pin));
        assert!(!codex.matches(&opus_alias));
    }
}
