pub(crate) fn seconds_to_ceil_ms(value: &str) -> Option<u64> {
    let value = value.trim();
    let (mantissa, exponent) = match value.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i64>().ok()?),
        None => (value, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || (mantissa.contains('.') && fraction.is_empty())
    {
        return None;
    }
    let digits = whole.bytes().chain(fraction.bytes()).collect::<Vec<_>>();
    let Some(first) = digits.iter().position(|byte| *byte != b'0') else {
        return Some(0);
    };
    let integer_digits = i64::try_from(whole.len())
        .ok()?
        .checked_add(exponent)?
        .checked_add(3)?
        .checked_sub(i64::try_from(first).ok()?)?;
    if integer_digits <= 0 {
        return Some(1);
    }
    if integer_digits > 20 {
        return None;
    }
    let significant = &digits[first..];
    let integer_digits = usize::try_from(integer_digits).ok()?;
    let mut millis = 0_u64;
    for index in 0..integer_digits {
        let digit = significant.get(index).copied().unwrap_or(b'0') - b'0';
        millis = millis.checked_mul(10)?.checked_add(u64::from(digit))?;
    }
    if significant
        .iter()
        .skip(integer_digits)
        .any(|byte| *byte != b'0')
    {
        millis = millis.checked_add(1)?;
    }
    Some(millis)
}
