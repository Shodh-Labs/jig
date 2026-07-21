//! Stable machine-readable class codes for [`Finding`](super::Finding).
//!
//! A finding's `message` is prose written for a human: it interpolates tool
//! names, token counts and millisecond timings, and it gets reworded whenever
//! the advice gets better. That makes it useless as an identity key, which is
//! exactly the problem the census-v2 aggregator hit — it had to *synthesise*
//! class keys by normalizing message text (backticked identifiers to a
//! placeholder, digit runs to a placeholder), so its class table fragments
//! whenever a message embeds an un-backticked value and is not comparable
//! across jig versions.
//!
//! [`FindingCode`] is the fix: a closed enum with one variant per defect class
//! the engine can emit, and a stable string form that never changes.
//!
//! # The string form is a public API surface
//!
//! Every code renders as `<dimension>.<class>`, where `<dimension>` is exactly
//! the finding's own [`Dimension::key`](super::Dimension::key) (`protocol`,
//! `context_cost`, `schema_hygiene`, `description_quality`, `robustness`,
//! `tool_set`, `injection`) and `<class>` is `snake_case`. That prefix is not
//! decoration: it means a consumer can group by dimension without a lookup
//! table, and it is enforced by a test.
//!
//! **These strings are published in `jig check --json` and consumers key off
//! them.** Renaming one silently breaks every dataset and dashboard built on
//! it. Treat the string form as frozen once released: add new variants freely,
//! but change an existing string only as a deliberate, announced break. The
//! exact set is pinned in a test (`finding_code_strings_are_pinned`) so an
//! accidental rename fails loudly rather than shipping.
//!
//! Adding a *new* class is additive and safe — consumers already have to cope
//! with codes they do not recognise, because they may be reading output from a
//! newer jig than they were written against.

use std::fmt;
use std::str::FromStr;

/// The class of a [`Finding`](super::Finding): a stable, machine-readable name
/// for *what kind of defect this is*, independent of how the message happens to
/// be worded.
///
/// A finding's `message` is prose: it interpolates tool names, token counts and
/// timings, and it is reworded whenever the advice improves. That makes it
/// useless as an identity key — the census-v2 aggregator had to *synthesise*
/// class keys by normalizing message text, which fragments whenever a message
/// embeds an un-backticked value and is not comparable across jig versions.
/// This enum is what a consumer should group and compare on instead.
///
/// # The string form
///
/// Every code renders as `<dimension>.<class>`, where `<dimension>` is exactly
/// the finding's own [`Dimension::key`](super::Dimension::key) (`protocol`,
/// `context_cost`, `schema_hygiene`, `description_quality`, `robustness`,
/// `tool_set`, `injection`) and `<class>` is `snake_case`. The prefix is not
/// decoration: it lets a consumer group by dimension without a lookup table,
/// and it is enforced by a test.
///
/// **These strings are a published API surface.** `jig check --json` emits them
/// as each finding's `code`, and downstream datasets and dashboards key off
/// them, so renaming one silently breaks every consumer. Treat the string form
/// as frozen once released: add new variants freely — a consumer must already
/// cope with codes from a newer jig than it was written against — but change an
/// existing string only as a deliberate, announced break. The exact set is
/// pinned in a test so an accidental rename fails loudly rather than shipping.
///
/// Round-trips through its string form: `code.as_str().parse()` yields `code`
/// back for every variant.
///
/// ```
/// use jig_core::FindingCode;
///
/// assert_eq!(FindingCode::ProtocolStdoutPollution.as_str(), "protocol.stdout_pollution");
/// assert_eq!(
///     "robustness.slow_boot".parse::<FindingCode>().unwrap(),
///     FindingCode::RobustnessSlowBoot
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum FindingCode {
    // -- Protocol compliance -------------------------------------------------
    /// Non-protocol bytes on stdout, breaking JSON-RPC framing.
    ProtocolStdoutPollution,
    /// A capability advertised outside the negotiated spec revision.
    ProtocolOffspecCapability,
    /// The `initialize` result is missing or empties a required field.
    ProtocolInitializeFieldInvalid,
    /// A tool name violates the MCP name format (SEP-986).
    ProtocolToolNameFormat,
    /// An unknown method was rejected, but with the wrong JSON-RPC error code.
    ProtocolUnknownMethodWrongCode,
    /// An unknown method was *accepted* instead of rejected with `-32601`.
    ProtocolUnknownMethodAccepted,
    /// A list operation was accepted but never answered.
    ProtocolListTimeout,
    /// The composite score was capped by a high-severity protocol defect.
    ProtocolCompositeCap,

    // -- Context cost --------------------------------------------------------
    /// The tool surface is heavy enough to be worth trimming.
    ContextCostHeavySurface,
    /// The composite score was capped by context cost.
    ContextCostCompositeCap,

    // -- Schema hygiene ------------------------------------------------------
    /// A tool carries no description at all.
    SchemaHygieneToolMissingDescription,
    /// One or more of a tool's parameters have no description.
    SchemaHygieneParamMissingDescription,
    /// One or more of a tool's parameters have no type (nor enum/`$ref`).
    SchemaHygieneParamMissingType,
    /// Tools declare no behavioural annotations (`readOnlyHint`, …).
    SchemaHygieneMissingAnnotations,

    // -- Description quality -------------------------------------------------
    /// A tool name contains whitespace, so models cannot call it.
    DescriptionQualityNameHasWhitespace,
    /// A tool name breaks the server's dominant naming convention.
    DescriptionQualityNameConventionInconsistent,
    /// A description is too terse for a model to select on.
    DescriptionQualityDescriptionTerse,
    /// A description is long enough to waste context.
    DescriptionQualityDescriptionVerbose,
    /// Tools carry no human-facing `title`.
    DescriptionQualityMissingTitle,

    // -- Robustness ----------------------------------------------------------
    /// `tools/list` was slower than the timing curve's top band.
    RobustnessSlowList,
    /// Server boot (launch to `initialize` response) was slow.
    RobustnessSlowBoot,
    /// The server did not shut down cleanly.
    RobustnessUncleanShutdown,
    /// The server wrote to stderr. Informational; never scored.
    RobustnessStderrNoise,
    /// Startup failed and the message named the missing credential variable —
    /// the good shape. Informational; never scored.
    RobustnessCredentialFailureNamedVariable,
    /// Startup failed without naming the missing credential variable.
    RobustnessCredentialFailureUnnamedVariable,
    /// The server hung rather than failing on a missing credential.
    RobustnessCredentialFailureHang,
    /// The server exited **zero** on a failed startup.
    RobustnessCredentialFailureExitedZero,

    // -- Tool set (advisory) -------------------------------------------------
    /// Two tool names denote the same action in interchangeable words.
    ToolSetNameCollision,
    /// One tool name is the other plus only generic words.
    ToolSetNameGenericSubset,
    /// Two tool descriptions overlap enough to be indistinguishable.
    ToolSetDescriptionOverlap,
    /// The pairwise collision scan hit its pair cap. Informational.
    ToolSetCollisionScanCapped,
    /// The tool count is past the published tool-selection accuracy cliff.
    ToolSetAccuracyCliff,
    /// A single tool dominates the surface's token bill.
    ToolSetCostDominantTool,
    /// The three heaviest tools carry most of the surface's token bill.
    ToolSetCostConcentration,

    // -- Prompt injection (advisory) -----------------------------------------
    /// A description carries a model-directed instruction.
    InjectionImperativeInstruction,
    /// A description embeds chat-template control tokens.
    InjectionFakeTurnControlToken,
    /// A description embeds role/instruction tags.
    InjectionFakeTurnRoleTag,
    /// A description contains a simulated conversation transcript.
    InjectionFakeTurnTranscript,
    /// A name or description contains zero-width characters.
    InjectionZeroWidthCharacters,
    /// A name or description contains Unicode bidirectional controls.
    InjectionBidiControls,
    /// A tool name uses non-ASCII characters that impersonate ASCII ones.
    InjectionHomoglyphName,
    /// A description pairs a hard-coded URL with an outbound-transfer verb.
    InjectionExfiltrationShape,
    /// A read-shaped tool name sits over a description that mutates.
    InjectionReadNameWriteBehaviour,
    /// `readOnlyHint: true` contradicts a mutating description.
    InjectionReadOnlyHintContradicted,
}

impl FindingCode {
    /// Every code the engine can emit, in dimension order.
    ///
    /// Exhaustive by construction: the test suite asserts this array covers the
    /// enum, so a new variant that is not listed here fails the build's tests.
    pub const ALL: &'static [FindingCode] = &[
        FindingCode::ProtocolStdoutPollution,
        FindingCode::ProtocolOffspecCapability,
        FindingCode::ProtocolInitializeFieldInvalid,
        FindingCode::ProtocolToolNameFormat,
        FindingCode::ProtocolUnknownMethodWrongCode,
        FindingCode::ProtocolUnknownMethodAccepted,
        FindingCode::ProtocolListTimeout,
        FindingCode::ProtocolCompositeCap,
        FindingCode::ContextCostHeavySurface,
        FindingCode::ContextCostCompositeCap,
        FindingCode::SchemaHygieneToolMissingDescription,
        FindingCode::SchemaHygieneParamMissingDescription,
        FindingCode::SchemaHygieneParamMissingType,
        FindingCode::SchemaHygieneMissingAnnotations,
        FindingCode::DescriptionQualityNameHasWhitespace,
        FindingCode::DescriptionQualityNameConventionInconsistent,
        FindingCode::DescriptionQualityDescriptionTerse,
        FindingCode::DescriptionQualityDescriptionVerbose,
        FindingCode::DescriptionQualityMissingTitle,
        FindingCode::RobustnessSlowList,
        FindingCode::RobustnessSlowBoot,
        FindingCode::RobustnessUncleanShutdown,
        FindingCode::RobustnessStderrNoise,
        FindingCode::RobustnessCredentialFailureNamedVariable,
        FindingCode::RobustnessCredentialFailureUnnamedVariable,
        FindingCode::RobustnessCredentialFailureHang,
        FindingCode::RobustnessCredentialFailureExitedZero,
        FindingCode::ToolSetNameCollision,
        FindingCode::ToolSetNameGenericSubset,
        FindingCode::ToolSetDescriptionOverlap,
        FindingCode::ToolSetCollisionScanCapped,
        FindingCode::ToolSetAccuracyCliff,
        FindingCode::ToolSetCostDominantTool,
        FindingCode::ToolSetCostConcentration,
        FindingCode::InjectionImperativeInstruction,
        FindingCode::InjectionFakeTurnControlToken,
        FindingCode::InjectionFakeTurnRoleTag,
        FindingCode::InjectionFakeTurnTranscript,
        FindingCode::InjectionZeroWidthCharacters,
        FindingCode::InjectionBidiControls,
        FindingCode::InjectionHomoglyphName,
        FindingCode::InjectionExfiltrationShape,
        FindingCode::InjectionReadNameWriteBehaviour,
        FindingCode::InjectionReadOnlyHintContradicted,
    ];

    /// The stable string form, `<dimension>.<class>`.
    ///
    /// **Published API.** `jig check --json` emits this string as a finding's
    /// `code`, and consumers key off it — see [`FindingCode`]'s own
    /// documentation before changing one.
    pub const fn as_str(self) -> &'static str {
        match self {
            FindingCode::ProtocolStdoutPollution => "protocol.stdout_pollution",
            FindingCode::ProtocolOffspecCapability => "protocol.offspec_capability",
            FindingCode::ProtocolInitializeFieldInvalid => "protocol.initialize_field_invalid",
            FindingCode::ProtocolToolNameFormat => "protocol.tool_name_format",
            FindingCode::ProtocolUnknownMethodWrongCode => "protocol.unknown_method_wrong_code",
            FindingCode::ProtocolUnknownMethodAccepted => "protocol.unknown_method_accepted",
            FindingCode::ProtocolListTimeout => "protocol.list_timeout",
            FindingCode::ProtocolCompositeCap => "protocol.composite_cap",

            FindingCode::ContextCostHeavySurface => "context_cost.heavy_surface",
            FindingCode::ContextCostCompositeCap => "context_cost.composite_cap",

            FindingCode::SchemaHygieneToolMissingDescription => {
                "schema_hygiene.tool_missing_description"
            }
            FindingCode::SchemaHygieneParamMissingDescription => {
                "schema_hygiene.param_missing_description"
            }
            FindingCode::SchemaHygieneParamMissingType => "schema_hygiene.param_missing_type",
            FindingCode::SchemaHygieneMissingAnnotations => "schema_hygiene.missing_annotations",

            FindingCode::DescriptionQualityNameHasWhitespace => {
                "description_quality.name_has_whitespace"
            }
            FindingCode::DescriptionQualityNameConventionInconsistent => {
                "description_quality.name_convention_inconsistent"
            }
            FindingCode::DescriptionQualityDescriptionTerse => {
                "description_quality.description_terse"
            }
            FindingCode::DescriptionQualityDescriptionVerbose => {
                "description_quality.description_verbose"
            }
            FindingCode::DescriptionQualityMissingTitle => "description_quality.missing_title",

            FindingCode::RobustnessSlowList => "robustness.slow_list",
            FindingCode::RobustnessSlowBoot => "robustness.slow_boot",
            FindingCode::RobustnessUncleanShutdown => "robustness.unclean_shutdown",
            FindingCode::RobustnessStderrNoise => "robustness.stderr_noise",
            FindingCode::RobustnessCredentialFailureNamedVariable => {
                "robustness.credential_failure_named_variable"
            }
            FindingCode::RobustnessCredentialFailureUnnamedVariable => {
                "robustness.credential_failure_unnamed_variable"
            }
            FindingCode::RobustnessCredentialFailureHang => "robustness.credential_failure_hang",
            FindingCode::RobustnessCredentialFailureExitedZero => {
                "robustness.credential_failure_exited_zero"
            }

            FindingCode::ToolSetNameCollision => "tool_set.name_collision",
            FindingCode::ToolSetNameGenericSubset => "tool_set.name_generic_subset",
            FindingCode::ToolSetDescriptionOverlap => "tool_set.description_overlap",
            FindingCode::ToolSetCollisionScanCapped => "tool_set.collision_scan_capped",
            FindingCode::ToolSetAccuracyCliff => "tool_set.accuracy_cliff",
            FindingCode::ToolSetCostDominantTool => "tool_set.cost_dominant_tool",
            FindingCode::ToolSetCostConcentration => "tool_set.cost_concentration",

            FindingCode::InjectionImperativeInstruction => "injection.imperative_instruction",
            FindingCode::InjectionFakeTurnControlToken => "injection.fake_turn_control_token",
            FindingCode::InjectionFakeTurnRoleTag => "injection.fake_turn_role_tag",
            FindingCode::InjectionFakeTurnTranscript => "injection.fake_turn_transcript",
            FindingCode::InjectionZeroWidthCharacters => "injection.zero_width_characters",
            FindingCode::InjectionBidiControls => "injection.bidi_controls",
            FindingCode::InjectionHomoglyphName => "injection.homoglyph_name",
            FindingCode::InjectionExfiltrationShape => "injection.exfiltration_shape",
            FindingCode::InjectionReadNameWriteBehaviour => "injection.read_name_write_behaviour",
            FindingCode::InjectionReadOnlyHintContradicted => {
                "injection.read_only_hint_contradicted"
            }
        }
    }
}

impl fmt::Display for FindingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error [`FindingCode::from_str`] returns for a string that names no known
/// class — most often output from a *newer* jig than this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownFindingCode(pub String);

impl fmt::Display for UnknownFindingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown finding code `{}`", self.0)
    }
}

impl std::error::Error for UnknownFindingCode {}

impl FromStr for FindingCode {
    type Err = UnknownFindingCode;

    /// Parse a stable string form back into its variant. Linear over
    /// [`ALL`](FindingCode::ALL) — 40-odd comparisons, which is cheaper than
    /// maintaining a second hand-written table that could drift out of step
    /// with [`as_str`](FindingCode::as_str).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        FindingCode::ALL
            .iter()
            .copied()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| UnknownFindingCode(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::Dimension;
    use std::collections::BTreeSet;

    /// The exact published string of every finding class, pinned.
    ///
    /// **This test is the public API contract.** If it fails because a string
    /// changed, that is a breaking change for every consumer of
    /// `jig check --json` — including the census aggregator — and the fix is
    /// almost always to restore the old string, not to update this list. Adding
    /// a line for a genuinely new class is fine.
    #[test]
    fn finding_code_strings_are_pinned() {
        let expected = [
            "protocol.stdout_pollution",
            "protocol.offspec_capability",
            "protocol.initialize_field_invalid",
            "protocol.tool_name_format",
            "protocol.unknown_method_wrong_code",
            "protocol.unknown_method_accepted",
            "protocol.list_timeout",
            "protocol.composite_cap",
            "context_cost.heavy_surface",
            "context_cost.composite_cap",
            "schema_hygiene.tool_missing_description",
            "schema_hygiene.param_missing_description",
            "schema_hygiene.param_missing_type",
            "schema_hygiene.missing_annotations",
            "description_quality.name_has_whitespace",
            "description_quality.name_convention_inconsistent",
            "description_quality.description_terse",
            "description_quality.description_verbose",
            "description_quality.missing_title",
            "robustness.slow_list",
            "robustness.slow_boot",
            "robustness.unclean_shutdown",
            "robustness.stderr_noise",
            "robustness.credential_failure_named_variable",
            "robustness.credential_failure_unnamed_variable",
            "robustness.credential_failure_hang",
            "robustness.credential_failure_exited_zero",
            "tool_set.name_collision",
            "tool_set.name_generic_subset",
            "tool_set.description_overlap",
            "tool_set.collision_scan_capped",
            "tool_set.accuracy_cliff",
            "tool_set.cost_dominant_tool",
            "tool_set.cost_concentration",
            "injection.imperative_instruction",
            "injection.fake_turn_control_token",
            "injection.fake_turn_role_tag",
            "injection.fake_turn_transcript",
            "injection.zero_width_characters",
            "injection.bidi_controls",
            "injection.homoglyph_name",
            "injection.exfiltration_shape",
            "injection.read_name_write_behaviour",
            "injection.read_only_hint_contradicted",
        ];
        let actual: Vec<&str> = FindingCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            actual, expected,
            "finding code strings are a published API surface — see check::code module docs"
        );
    }

    #[test]
    fn every_code_string_is_unique() {
        let set: BTreeSet<&str> = FindingCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            set.len(),
            FindingCode::ALL.len(),
            "two variants share a string form"
        );
    }

    #[test]
    fn code_strings_round_trip_through_parsing() {
        for code in FindingCode::ALL {
            assert_eq!(code.as_str().parse::<FindingCode>().unwrap(), *code);
        }
        assert!("protocol.not_a_real_class".parse::<FindingCode>().is_err());
    }

    #[test]
    fn every_code_is_prefixed_with_a_real_dimension_key() {
        let keys: BTreeSet<&str> = [
            Dimension::Protocol,
            Dimension::ContextCost,
            Dimension::SchemaHygiene,
            Dimension::DescriptionQuality,
            Dimension::Robustness,
            Dimension::ToolSet,
            Dimension::Injection,
        ]
        .iter()
        .map(|d| d.key())
        .collect();
        for code in FindingCode::ALL {
            let s = code.as_str();
            let (prefix, class) = s.split_once('.').expect("codes are `<dimension>.<class>`");
            assert!(keys.contains(prefix), "`{s}` has no such dimension prefix");
            assert!(
                !class.is_empty()
                    && class
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "`{s}` class part is not snake_case"
            );
        }
    }

    /// `ALL` must list every variant. Rust cannot prove that for us, so this
    /// match is the reminder: adding a variant makes it non-exhaustive and the
    /// crate stops compiling until the variant is added here *and* to `ALL`.
    #[test]
    fn all_lists_every_variant() {
        fn seen(c: FindingCode) -> usize {
            match c {
                FindingCode::ProtocolStdoutPollution
                | FindingCode::ProtocolOffspecCapability
                | FindingCode::ProtocolInitializeFieldInvalid
                | FindingCode::ProtocolToolNameFormat
                | FindingCode::ProtocolUnknownMethodWrongCode
                | FindingCode::ProtocolUnknownMethodAccepted
                | FindingCode::ProtocolListTimeout
                | FindingCode::ProtocolCompositeCap
                | FindingCode::ContextCostHeavySurface
                | FindingCode::ContextCostCompositeCap
                | FindingCode::SchemaHygieneToolMissingDescription
                | FindingCode::SchemaHygieneParamMissingDescription
                | FindingCode::SchemaHygieneParamMissingType
                | FindingCode::SchemaHygieneMissingAnnotations
                | FindingCode::DescriptionQualityNameHasWhitespace
                | FindingCode::DescriptionQualityNameConventionInconsistent
                | FindingCode::DescriptionQualityDescriptionTerse
                | FindingCode::DescriptionQualityDescriptionVerbose
                | FindingCode::DescriptionQualityMissingTitle
                | FindingCode::RobustnessSlowList
                | FindingCode::RobustnessSlowBoot
                | FindingCode::RobustnessUncleanShutdown
                | FindingCode::RobustnessStderrNoise
                | FindingCode::RobustnessCredentialFailureNamedVariable
                | FindingCode::RobustnessCredentialFailureUnnamedVariable
                | FindingCode::RobustnessCredentialFailureHang
                | FindingCode::RobustnessCredentialFailureExitedZero
                | FindingCode::ToolSetNameCollision
                | FindingCode::ToolSetNameGenericSubset
                | FindingCode::ToolSetDescriptionOverlap
                | FindingCode::ToolSetCollisionScanCapped
                | FindingCode::ToolSetAccuracyCliff
                | FindingCode::ToolSetCostDominantTool
                | FindingCode::ToolSetCostConcentration
                | FindingCode::InjectionImperativeInstruction
                | FindingCode::InjectionFakeTurnControlToken
                | FindingCode::InjectionFakeTurnRoleTag
                | FindingCode::InjectionFakeTurnTranscript
                | FindingCode::InjectionZeroWidthCharacters
                | FindingCode::InjectionBidiControls
                | FindingCode::InjectionHomoglyphName
                | FindingCode::InjectionExfiltrationShape
                | FindingCode::InjectionReadNameWriteBehaviour
                | FindingCode::InjectionReadOnlyHintContradicted => 1,
            }
        }
        assert_eq!(
            FindingCode::ALL.iter().map(|c| seen(*c)).sum::<usize>(),
            44,
            "FindingCode::ALL is missing a variant"
        );
    }
}
