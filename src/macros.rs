#![allow(clippy::redundant_pub_crate)]

macro_rules! inconceivable {
    () => {{
        #[cfg(debug_assertions)]
        {
            panic!("That word you use - I do not think it means what you think it means.");
        }
        #[allow(unreachable_code)]
        {
            std::hint::unreachable_unchecked();
        }
    }};
}

macro_rules! max {
    ($a:expr) => {{
        let mut idx = 0;
        let mut max = $a[0];
        while idx < $a.len() {
            max = max!(max, $a[idx]);
            idx += 1;
        }
        max
    }};
    ($a:expr, $b:expr) => {
        if $a > $b {
            $a
        } else {
            $b
        }
    };
}

macro_rules! min {
    ($a:expr) => {{
        let mut idx = 0;
        let mut min = $a[0];
        while idx < $a.len() {
            min = min!(min, $a[idx]);
            idx += 1;
        }
        min
    }};
    ($a:expr, $b:expr) => {
        if $a < $b {
            $a
        } else {
            $b
        }
    };
}

pub(crate) use inconceivable;
