use std::fmt;

/// Explicit query AST. Evaluation happens at file level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Term(String),
    Phrase(String),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}

impl Expr {
    /// Stable description for the UI query-plan banner.
    pub fn describe(&self) -> String {
        match self {
            Self::Term(value) => format!("Term({value:?})"),
            Self::Phrase(value) => format!("Phrase({value:?})"),
            Self::And(left, right) => format!("And({}, {})", left.describe(), right.describe()),
            Self::Or(left, right) => format!("Or({}, {})", left.describe(), right.describe()),
            Self::Not(inner) => format!("Not({})", inner.describe()),
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.describe())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} at position {position}")]
pub struct ParseError {
    pub message: String,
    pub position: usize,
}

impl ParseError {
    fn new(message: impl Into<String>, position: usize) -> Self {
        Self {
            message: message.into(),
            position,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String, usize),
    /// The position is the opening quote.
    Phrase(String, usize),
    Open(usize),
    Close(usize),
    And(usize),
    Or(usize),
    Not(usize),
    Minus(usize),
}

impl Token {
    fn position(&self) -> usize {
        match self {
            Self::Word(_, position)
            | Self::Phrase(_, position)
            | Self::Open(position)
            | Self::Close(position)
            | Self::And(position)
            | Self::Or(position)
            | Self::Not(position)
            | Self::Minus(position) => *position,
        }
    }

    fn starts_operand(&self) -> bool {
        matches!(
            self,
            Self::Word(..) | Self::Phrase(..) | Self::Open(_) | Self::Not(_) | Self::Minus(_)
        )
    }

    fn describe(&self) -> &'static str {
        match self {
            Self::Word(..) => "term",
            Self::Phrase(..) => "phrase",
            Self::Open(_) => "'('",
            Self::Close(_) => "')'",
            Self::And(_) => "AND",
            Self::Or(_) => "OR",
            Self::Not(_) => "NOT",
            Self::Minus(_) => "'-'",
        }
    }
}

pub fn parse(input: &str) -> Result<Expr, ParseError> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err(ParseError::new("query is empty", 0));
    }
    let mut parser = Parser {
        tokens,
        position: 0,
        end: input.len(),
    };
    let expr = parser.parse_or()?;
    if let Some(token) = parser.peek() {
        return Err(ParseError::new(
            format!("unexpected {}", token.describe()),
            token.position(),
        ));
    }
    Ok(expr)
}

fn tokenize(input: &str) -> Result<Vec<Token>, ParseError> {
    let mut tokens = Vec::new();
    let mut position = 0usize;
    let mut group_depth = 0usize;

    while position < input.len() {
        let ch = input[position..].chars().next().expect("in bounds");
        if ch.is_whitespace() {
            position += ch.len_utf8();
            continue;
        }
        match ch {
            '(' => {
                tokens.push(Token::Open(position));
                group_depth += 1;
                position += 1;
            }
            ')' => {
                if group_depth == 0 {
                    return Err(ParseError::new("unexpected ')'", position));
                }
                tokens.push(Token::Close(position));
                group_depth -= 1;
                position += 1;
            }
            '"' => {
                let start = position;
                position += 1;
                let mut value = String::new();
                let mut closed = false;
                while position < input.len() {
                    let current = input[position..].chars().next().expect("in bounds");
                    if current == '\\' {
                        position += 1;
                        if position >= input.len() {
                            break;
                        }
                        let escaped = input[position..].chars().next().expect("in bounds");
                        if escaped == '"' || escaped == '\\' {
                            value.push(escaped);
                            position += escaped.len_utf8();
                            continue;
                        }
                        value.push('\\');
                        value.push(escaped);
                        position += escaped.len_utf8();
                        continue;
                    }
                    if current == '"' {
                        position += 1;
                        closed = true;
                        break;
                    }
                    value.push(current);
                    position += current.len_utf8();
                }
                if !closed {
                    return Err(ParseError::new("unterminated quote", start));
                }
                if value.is_empty() {
                    return Err(ParseError::new("empty quoted phrase", start));
                }
                tokens.push(Token::Phrase(value, start));
            }
            '-' => {
                tokens.push(Token::Minus(position));
                position += 1;
            }
            _ => {
                let start = position;
                let mut word = String::new();
                let mut literal_parens = 0usize;
                while position < input.len() {
                    let current = input[position..].chars().next().expect("in bounds");
                    if current.is_whitespace() || current == '"' {
                        break;
                    }
                    if current == '(' && !word.is_empty() {
                        // Function-call / grouping punctuation is part of the
                        // literal, e.g. `dma_alloc_coherent()`.
                        word.push('(');
                        literal_parens += 1;
                        position += 1;
                        continue;
                    }
                    if current == ')' {
                        if literal_parens > 0 {
                            word.push(')');
                            literal_parens -= 1;
                            position += 1;
                            continue;
                        }
                        if group_depth > 0 {
                            break;
                        }
                        word.push(')');
                        position += 1;
                        continue;
                    }
                    word.push(current);
                    position += current.len_utf8();
                }
                if word.is_empty() {
                    return Err(ParseError::new("unexpected character", start));
                }
                let token = match word.as_str() {
                    "AND" => Token::And(start),
                    "OR" => Token::Or(start),
                    "NOT" => Token::Not(start),
                    _ => Token::Word(word, start),
                };
                tokens.push(token);
            }
        }
    }

    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        if token.is_some() {
            self.position += 1;
        }
        token
    }

    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_and()?;
        while let Some(Token::Or(position)) = self.peek() {
            let position = *position;
            self.position += 1;
            let right = self.parse_and().map_err(|error| {
                if error.message.starts_with("expected") {
                    ParseError::new("expected expression after OR", position)
                } else {
                    error
                }
            })?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_unary()?;
        loop {
            match self.peek() {
                Some(Token::And(_)) => {
                    self.position += 1;
                    let right = self.parse_unary()?;
                    left = Expr::And(Box::new(left), Box::new(right));
                }
                Some(token) if token.starts_operand() => {
                    let right = self.parse_unary()?;
                    left = Expr::And(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        match self.peek() {
            Some(Token::Not(_)) | Some(Token::Minus(_)) => {
                let position = self.peek().expect("checked").position();
                self.position += 1;
                let inner = self.parse_unary().map_err(|error| {
                    if error.message.starts_with("expected") {
                        ParseError::new("expected expression after negation", position)
                    } else {
                        error
                    }
                })?;
                Ok(Expr::Not(Box::new(inner)))
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        match self.next() {
            Some(Token::Word(value, _)) => Ok(Expr::Term(value)),
            Some(Token::Phrase(value, _)) => Ok(Expr::Phrase(value)),
            Some(Token::Open(position)) => {
                let inner = self.parse_or()?;
                match self.next() {
                    Some(Token::Close(_)) => Ok(inner),
                    _ => Err(ParseError::new("unclosed '('", position)),
                }
            }
            Some(token) => Err(ParseError::new(
                format!("expected expression, found {}", token.describe()),
                token.position(),
            )),
            None => Err(ParseError::new("expected expression", self.end)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implicit_and_and_negation() {
        let expr = parse("Linux -Windows").unwrap();
        assert_eq!(
            expr,
            Expr::And(
                Box::new(Expr::Term("Linux".into())),
                Box::new(Expr::Not(Box::new(Expr::Term("Windows".into()))))
            )
        );
    }

    #[test]
    fn parentheses_and_or() {
        let expr = parse("(Linux OR Windows) 驱动").unwrap();
        assert_eq!(
            expr,
            Expr::And(
                Box::new(Expr::Or(
                    Box::new(Expr::Term("Linux".into())),
                    Box::new(Expr::Term("Windows".into()))
                )),
                Box::new(Expr::Term("驱动".into()))
            )
        );
    }

    #[test]
    fn code_punctuation_stays_literal() {
        assert_eq!(
            parse("dma_alloc_coherent()").unwrap(),
            Expr::Term("dma_alloc_coherent()".into())
        );
        assert_eq!(parse("#include").unwrap(), Expr::Term("#include".into()));
        assert_eq!(parse("100%").unwrap(), Expr::Term("100%".into()));
        assert_eq!(parse("a_b").unwrap(), Expr::Term("a_b".into()));
        assert_eq!(
            parse("Linux-Windows").unwrap(),
            Expr::Term("Linux-Windows".into())
        );
    }

    #[test]
    fn phrases_keep_spaces() {
        assert_eq!(
            parse("\"Linux kernel\"").unwrap(),
            Expr::Phrase("Linux kernel".into())
        );
    }

    #[test]
    fn errors_carry_positions() {
        let error = parse("(Linux OR Windows").unwrap_err();
        assert_eq!(error.position, 0);
        assert!(error.message.contains("unclosed"));

        let error = parse("Linux OR").unwrap_err();
        assert_eq!(error.position, 6);

        let error = parse("\"unterminated").unwrap_err();
        assert_eq!(error.position, 0);

        let error = parse(")").unwrap_err();
        assert_eq!(error.position, 0);
    }

    #[test]
    fn function_call_inside_group() {
        assert_eq!(
            parse("(alpha OR beta()) gamma").unwrap(),
            Expr::And(
                Box::new(Expr::Or(
                    Box::new(Expr::Term("alpha".into())),
                    Box::new(Expr::Term("beta()".into()))
                )),
                Box::new(Expr::Term("gamma".into()))
            )
        );
    }
}
