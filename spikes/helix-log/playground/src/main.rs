//! Throwaway code for the helix-log spike. Edit freely.

use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Num(f64),
    Ident(String),
    Op(char),
    LParen,
    RParen,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Num(n) => write!(f, "{n}"),
            Token::Ident(s) => write!(f, "{s}"),
            Token::Op(c) => write!(f, "{c}"),
            Token::LParen => write!(f, "("),
            Token::RParen => write!(f, ")"),
        }
    }
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || c == '.' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            let n = text.parse().map_err(|_| format!("bad number: {text}"))?;
            tokens.push(Token::Num(n));
        } else if c.is_alphabetic() {
            let start = i;
            while i < chars.len() && chars[i].is_alphanumeric() {
                i += 1;
            }
            tokens.push(Token::Ident(chars[start..i].iter().collect()));
        } else if "+-*/".contains(c) {
            tokens.push(Token::Op(c));
            i += 1;
        } else if c == '(' {
            tokens.push(Token::LParen);
            i += 1;
        } else if c == ')' {
            tokens.push(Token::RParen);
            i += 1;
        } else {
            return Err(format!("unexpected character: {c}"));
        }
    }
    Ok(tokens)
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    vars: &'a HashMap<String, f64>,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token], vars: &'a HashMap<String, f64>) -> Self {
        Parser { tokens, pos: 0, vars }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn expr(&mut self) -> Result<f64, String> {
        let mut value = self.term()?;
        while let Some(Token::Op(op @ ('+' | '-'))) = self.peek().cloned() {
            self.next();
            let rhs = self.term()?;
            value = if op == '+' { value + rhs } else { value - rhs };
        }
        Ok(value)
    }

    fn term(&mut self) -> Result<f64, String> {
        let mut value = self.atom()?;
        while let Some(Token::Op(op @ ('*' | '/'))) = self.peek().cloned() {
            self.next();
            let rhs = self.atom()?;
            // Deliberate bug for the reviewer to find later: no divide-by-zero check.
            value = if op == '*' { value * rhs } else { value / rhs };
        }
        Ok(value)
    }

    fn atom(&mut self) -> Result<f64, String> {
        match self.next() {
            Some(Token::Num(n)) => Ok(n),
            Some(Token::Ident(name)) => self
                .vars
                .get(&name)
                .copied()
                .ok_or_else(|| format!("unknown variable: {name}")),
            Some(Token::Op('-')) => Ok(-self.atom()?),
            Some(Token::LParen) => {
                let v = self.expr()?;
                match self.next() {
                    Some(Token::RParen) => Ok(v),
                    other => Err(format!("expected ')', got {other:?}")),
                }
            }
            other => Err(format!("unexpected token: {other:?}")),
        }
    }
}

fn eval(input: &str, vars: &HashMap<String, f64>) -> Result<f64, String> {
    let tokens = tokenize(input)?;
    let mut parser = Parser::new(&tokens, vars);
    let value = parser.expr()?;
    if parser.pos < tokens.len() {
        return Err(format!("trailing input at token {}", tokens[parser.pos]));
    }
    Ok(value)
}

fn main() {
    let mut vars = HashMap::new();
    vars.insert("pi".to_string(), std::f64::consts::PI);
    vars.insert("x".to_string(), 4.0);

    for line in ["1 + 2 * 3", "(1 + 2) * 3", "pi * x * x", "-x / 2", "1 / 0", "2 +"] {
        match eval(line, &vars) {
            Ok(v) => println!("{line} = {v}"),
            Err(e) => println!("{line} -> error: {e}"),
        }
    }
}
