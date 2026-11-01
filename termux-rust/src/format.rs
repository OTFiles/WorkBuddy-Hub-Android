//! 数值的显示形态。
//!
//! 同一个数字在 CLI 和 TUI 里必须长得一样，所以格式化只在这里做一次。这也顺带
//! 解决了「积分到底该显示 1884 还是 1884.00」的问题：App 里写的是 `1884`，跟着走。

/// 积分的显示形态：整数不带小数点，否则保留两位。
pub fn credits(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

/// 右补空格到指定显示宽度。
///
/// 按**终端列数**而不是 char 数：一个汉字占两列，用 `{:<12}` 补出来的表格会一行
/// 比一行靠右。
pub fn pad(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let used = text.width();
    if used >= width {
        return text.to_string();
    }
    format!("{text}{}", " ".repeat(width - used))
}

/// 千分位，数字大了以后一眼能读出量级。
pub fn thousands(value: i64) -> String {
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if negative {
        format!("-{out}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_drop_a_zero_fraction() {
        assert_eq!(credits(1884.0), "1884");
        assert_eq!(credits(0.0), "0");
        assert_eq!(credits(0.5), "0.50");
        assert_eq!(credits(12.345), "12.35");
        assert_eq!(credits(-3.0), "-3");
    }

    #[test]
    fn thousands_group_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(2338506), "2,338,506");
        assert_eq!(thousands(-1234), "-1,234");
    }

    #[test]
    fn padding_uses_terminal_columns() {
        // 两个汉字占四列，补到 6 列只需要两个空格。
        assert_eq!(pad("账号", 6), "账号  ");
        assert_eq!(pad("abc", 6), "abc   ");
        assert_eq!(pad("abcdef", 3), "abcdef");
    }
}
