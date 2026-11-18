use std::panic::catch_unwind;

fn parse(input: &str) {
    assert!(input.parse::<u32>().is_ok());
}

#[test]
fn expected_panics() {
    for input in ["invalid", "-1"] {
        assert!(catch_unwind(|| parse(input)).is_err());
    }
}

#[test]
fn expected_panics_bound() {
    for input in ["invalid", "-1"] {
        let result = catch_unwind(|| parse(input));
        assert!(result.is_err());
    }
}

#[test]
fn expected_panics_with_context() {
    for input in ["invalid", "-1"] {
        catch_unwind(|| parse(input)).expect_err(input);
    }
}

#[test]
fn propagate_failure() -> std::thread::Result<()> {
    for input in ["1", "2"] {
        catch_unwind(|| parse(input))?;
    }
    Ok(())
}

#[test]
fn resume_failure() {
    for input in ["1", "2"] {
        catch_unwind(|| parse(input)).unwrap_or_else(|payload| std::panic::resume_unwind(payload));
    }
}

#[test]
fn bound_unwrap_propagates_failure() {
    for input in ["1", "2"] {
        let result = catch_unwind(|| parse(input));
        result.unwrap();
    }
}

#[test]
fn bound_expect_propagates_failure() {
    for input in ["1", "2"] {
        let result = catch_unwind(|| parse(input));
        result.expect(input);
    }
}

#[test]
fn bound_question_mark_propagates_failure() -> std::thread::Result<()> {
    for input in ["1", "2"] {
        let result = catch_unwind(|| parse(input));
        result?;
    }
    Ok(())
}

#[test]
fn bound_resume_unwind_propagates_failure() {
    for input in ["1", "2"] {
        let result = catch_unwind(|| parse(input));
        result.unwrap_or_else(|payload| std::panic::resume_unwind(payload));
    }
}

#[test]
fn bound_fallback_swallows_failure() {
    for input in ["1", "2"] {
        let result = catch_unwind(|| parse(input));
        result.unwrap_or_else(|_| ());
    }
}

#[test]
fn fallback_after_failure() {
    for input in ["1", "2"] {
        catch_unwind(|| parse(input)).unwrap_or_else(|_| ());
    }
}

#[test]
fn swallow_failure() {
    for _ in 0..2 {
        let _ = catch_unwind(|| parse("1"));
    }
}

#[test]
fn retry_until_success() {
    for _ in 0..2 {
        if catch_unwind(|| parse("1")).is_ok() {
            break;
        }
    }
}

#[test]
fn swallow_failure_inside_assertion() {
    for _ in 0..2 {
        assert!({
            let _ = catch_unwind(|| parse("1"));
            true
        });
    }
}
