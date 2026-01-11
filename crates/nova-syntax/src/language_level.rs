//! Java language level + preview feature availability model.
//!
//! Nova parses a *superset* Java grammar (modern Java) and then gates language
//! features based on the configured per-project/module language level.
//!
//! This module is the canonical source of truth for "which Java version enables
//! which feature?", and is used by syntax feature gating and (eventually) semantic
//! analysis.

/// The effective Java language mode for a module/file.
///
/// - `major`: the Java feature release number (8, 11, 17, 21, 22, …)
/// - `preview`: whether `--enable-preview` is in effect for this major version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JavaLanguageLevel {
    pub major: u16,
    pub preview: bool,
}

impl JavaLanguageLevel {
    pub const JAVA_8: Self = Self {
        major: 8,
        preview: false,
    };
    pub const JAVA_11: Self = Self {
        major: 11,
        preview: false,
    };
    pub const JAVA_17: Self = Self {
        major: 17,
        preview: false,
    };
    pub const JAVA_21: Self = Self {
        major: 21,
        preview: false,
    };

    #[inline]
    pub const fn with_preview(self, preview: bool) -> Self {
        Self { preview, ..self }
    }

    pub fn availability(self, feature: JavaFeature) -> FeatureAvailability {
        use FeatureAvailability::*;
        use JavaFeature::*;

        match feature {
            Modules => {
                if self.major >= 9 {
                    Stable
                } else {
                    Unavailable
                }
            }

            VarLocalInference => {
                if self.major >= 10 {
                    Stable
                } else {
                    Unavailable
                }
            }

            SwitchExpressions => {
                if self.major >= 14 {
                    Stable
                } else if self.major == 12 || self.major == 13 {
                    Preview
                } else {
                    Unavailable
                }
            }

            TextBlocks => {
                if self.major >= 15 {
                    Stable
                } else if self.major == 13 || self.major == 14 {
                    Preview
                } else {
                    Unavailable
                }
            }

            PatternMatchingInstanceof => {
                if self.major >= 16 {
                    Stable
                } else if self.major == 14 || self.major == 15 {
                    Preview
                } else {
                    Unavailable
                }
            }

            Records => {
                if self.major >= 16 {
                    Stable
                } else if self.major == 14 || self.major == 15 {
                    Preview
                } else {
                    Unavailable
                }
            }

            SealedClasses => {
                if self.major >= 17 {
                    Stable
                } else if self.major == 15 || self.major == 16 {
                    Preview
                } else {
                    Unavailable
                }
            }

            PatternMatchingSwitch => {
                if self.major >= 21 {
                    Stable
                } else if (17..=20).contains(&self.major) {
                    Preview
                } else {
                    Unavailable
                }
            }

            RecordPatterns => {
                if self.major >= 21 {
                    Stable
                } else if (19..=20).contains(&self.major) {
                    Preview
                } else {
                    Unavailable
                }
            }

            UnnamedVariables => {
                if self.major >= 22 {
                    Stable
                } else if self.major == 21 {
                    Preview
                } else {
                    Unavailable
                }
            }

            StringTemplates => {
                if self.major >= 21 {
                    Preview
                } else {
                    Unavailable
                }
            }
        }
    }

    /// Is the feature usable in this configuration? (applies `preview` flag)
    pub fn is_enabled(self, feature: JavaFeature) -> bool {
        match self.availability(feature) {
            FeatureAvailability::Stable => true,
            FeatureAvailability::Preview => self.preview,
            FeatureAvailability::Unavailable => false,
        }
    }

    #[inline]
    pub fn supports_modules(self) -> bool {
        self.is_enabled(JavaFeature::Modules)
    }

    #[inline]
    pub fn supports_var_local_inference(self) -> bool {
        self.is_enabled(JavaFeature::VarLocalInference)
    }

    #[inline]
    pub fn supports_switch_expressions(self) -> bool {
        self.is_enabled(JavaFeature::SwitchExpressions)
    }

    #[inline]
    pub fn supports_text_blocks(self) -> bool {
        self.is_enabled(JavaFeature::TextBlocks)
    }

    #[inline]
    pub fn supports_records(self) -> bool {
        self.is_enabled(JavaFeature::Records)
    }

    #[inline]
    pub fn supports_sealed(self) -> bool {
        self.is_enabled(JavaFeature::SealedClasses)
    }

    #[inline]
    pub fn supports_pattern_matching_switch(self) -> bool {
        self.is_enabled(JavaFeature::PatternMatchingSwitch)
    }

    #[inline]
    pub fn supports_unnamed_variables(self) -> bool {
        self.is_enabled(JavaFeature::UnnamedVariables)
    }
}

impl Default for JavaLanguageLevel {
    fn default() -> Self {
        JavaLanguageLevel::JAVA_21
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JavaFeature {
    Modules,                   // Java 9+
    VarLocalInference,         // Java 10+
    SwitchExpressions,         // final Java 14 (preview earlier in javac, ignored here)
    TextBlocks,                // final Java 15
    PatternMatchingInstanceof, // final Java 16 (preview 14/15)
    Records,                   // final Java 16 (preview 14/15)
    SealedClasses,             // final Java 17 (preview 15/16)
    PatternMatchingSwitch,     // final Java 21 (preview 17-20)
    RecordPatterns,            // final Java 21 (preview earlier)
    UnnamedVariables,          // Java 22+ (preview in 21)
    StringTemplates,           // Java 21+ (preview)
}

impl JavaFeature {
    pub const fn diagnostic_code(self) -> &'static str {
        match self {
            JavaFeature::Modules => "JAVA_FEATURE_MODULES",
            JavaFeature::VarLocalInference => "JAVA_FEATURE_VAR_LOCAL_INFERENCE",
            JavaFeature::SwitchExpressions => "JAVA_FEATURE_SWITCH_EXPRESSIONS",
            JavaFeature::TextBlocks => "JAVA_FEATURE_TEXT_BLOCKS",
            JavaFeature::PatternMatchingInstanceof => "JAVA_FEATURE_PATTERN_MATCHING_INSTANCEOF",
            JavaFeature::Records => "JAVA_FEATURE_RECORDS",
            JavaFeature::SealedClasses => "JAVA_FEATURE_SEALED_CLASSES",
            JavaFeature::PatternMatchingSwitch => "JAVA_FEATURE_PATTERN_MATCHING_SWITCH",
            JavaFeature::RecordPatterns => "JAVA_FEATURE_RECORD_PATTERNS",
            JavaFeature::UnnamedVariables => "JAVA_FEATURE_UNNAMED_VARIABLES",
            JavaFeature::StringTemplates => "JAVA_FEATURE_STRING_TEMPLATES",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            JavaFeature::Modules => "modules",
            JavaFeature::VarLocalInference => "local variable type inference (`var`)",
            JavaFeature::SwitchExpressions => "switch expressions",
            JavaFeature::TextBlocks => "text blocks",
            JavaFeature::PatternMatchingInstanceof => "pattern matching for `instanceof`",
            JavaFeature::Records => "records",
            JavaFeature::SealedClasses => "sealed classes",
            JavaFeature::PatternMatchingSwitch => "pattern matching for switch",
            JavaFeature::RecordPatterns => "record patterns",
            JavaFeature::UnnamedVariables => "unnamed variables",
            JavaFeature::StringTemplates => "string templates",
        }
    }

    pub const fn stable_since(self) -> Option<u16> {
        match self {
            JavaFeature::Modules => Some(9),
            JavaFeature::VarLocalInference => Some(10),
            JavaFeature::SwitchExpressions => Some(14),
            JavaFeature::TextBlocks => Some(15),
            JavaFeature::PatternMatchingInstanceof => Some(16),
            JavaFeature::Records => Some(16),
            JavaFeature::SealedClasses => Some(17),
            JavaFeature::PatternMatchingSwitch => Some(21),
            JavaFeature::RecordPatterns => Some(21),
            JavaFeature::UnnamedVariables => Some(22),
            JavaFeature::StringTemplates => None,
        }
    }
}

/// Whether the *language* supports a feature in this major version,
/// independent of whether preview is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureAvailability {
    Unavailable,
    Preview,
    Stable,
}
