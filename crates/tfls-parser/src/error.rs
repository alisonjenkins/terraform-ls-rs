use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("HCL syntax error: {message}")]
    Syntax {
        message: String,
        #[source]
        source: hcl_edit::parser::Error,
    },

    #[error("failed to read file '{path}'")]
    FileRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("LSP position line {line} exceeds document line count {total_lines}")]
    LineOutOfBounds { line: u32, total_lines: usize },

    #[error("LSP position character {character} exceeds line {line} length")]
    CharacterOutOfBounds { line: u32, character: u32 },

    #[error("byte offset {offset} exceeds document length {length}")]
    ByteOffsetOutOfBounds { offset: usize, length: usize },

    #[error("terraform JSON syntax error: {message}")]
    Json { message: String },
}
