//! 自然排序：file2 < file10，Windows 资源管理器同款观感。

pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();

    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(ac), Some(bc)) => {
                if ac.is_ascii_digit() && bc.is_ascii_digit() {
                    let (na, yak) = take_number(&mut ai);
                    let (nb, ybk) = take_number(&mut bi);
                    let cmp = na.cmp(&nb);
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                    // 数值段相同则比较前导零数量，短者在前
                    if yak != ybk {
                        return yak.cmp(&ybk);
                    }
                } else {
                    let la = ac.to_lowercase().next().unwrap_or(ac);
                    let lb = bc.to_lowercase().next().unwrap_or(bc);
                    let cmp = la.cmp(&lb);
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                    ai.next();
                    bi.next();
                }
            }
        }
    }
}

/// 取出连续数字段并折算为 u128，同时返回前导零个数。
fn take_number<I: Iterator<Item = char>>(it: &mut std::iter::Peekable<I>) -> (u128, usize) {
    let mut digits = String::new();
    while let Some(c) = it.peek() {
        if c.is_ascii_digit() {
            digits.push(*c);
            it.next();
        } else {
            break;
        }
    }
    let zeros = digits.chars().take_while(|c| *c == '0').count();
    let value = digits.parse::<u128>().unwrap_or(u128::MAX);
    (value, zeros)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_chunks() {
        assert_eq!(natural_cmp("file2", "file10"), std::cmp::Ordering::Less);
        assert_eq!(natural_cmp("f10", "f2"), std::cmp::Ordering::Greater);
        assert_eq!(natural_cmp("a", "A"), std::cmp::Ordering::Equal);
    }
}
