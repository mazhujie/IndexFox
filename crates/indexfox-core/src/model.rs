use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Strict contiguous match; optional case sensitivity.
    Exact,
    /// Any-position substring match; ASCII case-insensitive by default.
    Substring,
    /// Substring candidates plus edit-distance verification.
    Fuzzy,
}

impl SearchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Substring => "substring",
            Self::Fuzzy => "fuzzy",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "exact" => Some(Self::Exact),
            "substring" => Some(Self::Substring),
            "fuzzy" => Some(Self::Fuzzy),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Local,
    Smb,
    Wsl,
    Removable,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Smb => "smb",
            Self::Wsl => "wsl",
            Self::Removable => "removable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    Indexed,
    Pending,
    Failed,
    Stale,
    SkippedSize,
    Offline,
}

impl FileState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Indexed => "indexed",
            Self::Pending => "pending",
            Self::Failed => "failed",
            Self::Stale => "stale",
            Self::SkippedSize => "skipped_size",
            Self::Offline => "offline",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    Relevance,
    ModifiedTime,
    Size,
    Path,
    Filename,
    HitCount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Native,
    Ocr,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Ocr => "ocr",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "ocr" => Self::Ocr,
            _ => Self::Native,
        }
    }
}

/// Where a [`Section`] lives inside its source file.
///
/// Offsets are byte offsets into the decoded section text. Line numbers are
/// 1-based; paragraph indexes are 0-based document order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceLocator {
    Text {
        start_line: u32,
        end_line: u32,
        start_offset: usize,
        end_offset: usize,
    },
    PdfPage {
        page: u32,
        #[serde(default)]
        bbox: Option<[f32; 4]>,
    },
    DocxParagraph {
        paragraph: u32,
    },
    DocxTableCell {
        table: u32,
        row: u32,
        column: u32,
        paragraph: u32,
    },
}

impl SourceLocator {
    pub fn page(&self) -> Option<u32> {
        match self {
            Self::PdfPage { page, .. } => Some(*page),
            _ => None,
        }
    }

    pub fn line(&self) -> Option<u32> {
        match self {
            Self::Text { start_line, .. } => Some(*start_line),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Section {
    /// Text used for matching (ASCII-lowercased, otherwise verbatim).
    pub text: String,
    /// Original decoded text used for display and highlight offsets.
    pub display_text: String,
    pub locator: SourceLocator,
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractedDocument {
    pub extractor_version: String,
    pub encoding: Option<String>,
    pub sections: Vec<Section>,
}

impl ExtractedDocument {
    pub fn char_len(&self) -> usize {
        self.sections.iter().map(|s| s.display_text.len()).sum()
    }
}
