use anyhow::{Context, Result, bail};

pub fn calculate(expression: &str) -> Result<String> {
    let mut parser = Parser {
        input: expression.as_bytes(),
        position: 0,
    };
    let value = parser.expression()?;
    parser.skip_whitespace();
    if parser.position != parser.input.len() {
        bail!("unexpected character at position {}", parser.position + 1);
    }
    if !value.is_finite() {
        bail!("result is not a finite number");
    }
    if value == 0.0 {
        return Ok("0".into());
    }
    if value.abs() >= 1_000_000_000_000.0 || value.abs() < 0.000_000_001 {
        return Ok(format!("{value:.10e}"));
    }
    Ok(format!("{value:.12}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned())
}

struct Parser<'a> {
    input: &'a [u8],
    position: usize,
}

impl Parser<'_> {
    fn expression(&mut self) -> Result<f64> {
        let mut value = self.term()?;
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b'+') => {
                    self.position += 1;
                    value += self.term()?;
                }
                Some(b'-') => {
                    self.position += 1;
                    value -= self.term()?;
                }
                _ => return Ok(value),
            }
        }
    }

    fn term(&mut self) -> Result<f64> {
        let mut value = self.factor()?;
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b'*') => {
                    self.position += 1;
                    value *= self.factor()?;
                }
                Some(b'/') => {
                    self.position += 1;
                    let divisor = self.factor()?;
                    if divisor == 0.0 {
                        bail!("division by zero")
                    }
                    value /= divisor;
                }
                _ => return Ok(value),
            }
        }
    }

    fn factor(&mut self) -> Result<f64> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'+') => {
                self.position += 1;
                self.factor()
            }
            Some(b'-') => {
                self.position += 1;
                Ok(-self.factor()?)
            }
            Some(b'(') => {
                self.position += 1;
                let value = self.expression()?;
                self.skip_whitespace();
                if self.peek() != Some(b')') {
                    bail!("missing closing parenthesis")
                }
                self.position += 1;
                Ok(value)
            }
            _ => self.number(),
        }
    }

    fn number(&mut self) -> Result<f64> {
        self.skip_whitespace();
        let start = self.position;
        let mut dot = false;
        while let Some(byte) = self.peek() {
            if byte.is_ascii_digit() {
                self.position += 1;
            } else if byte == b'.' && !dot {
                dot = true;
                self.position += 1;
            } else {
                break;
            }
        }
        if start == self.position {
            bail!("expected a number at position {}", start + 1)
        }
        let text = std::str::from_utf8(&self.input[start..self.position])?;
        text.parse::<f64>()
            .with_context(|| format!("invalid number `{text}`"))
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.position += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::calculate;

    #[test]
    fn respects_precedence_unary_and_errors() {
        assert_eq!(calculate("2 + 3 * (4 - 1)").unwrap(), "11");
        assert_eq!(calculate("-(3+2) / 2").unwrap(), "-2.5");
        assert!(calculate("1/0").is_err());
        assert!(calculate("1+foo").is_err());
    }
}
