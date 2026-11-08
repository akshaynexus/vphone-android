//! `/proc/meminfo`, and the defaults and limits derived from it.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MemInfo {
    pub total_mib: u64,
    pub available_mib: u64,
    pub swap_free_mib: u64,
}

impl MemInfo {
    pub fn parse(text: &str) -> MemInfo {
        let field = |name: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
                .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
                .map_or(0, |kib| kib / 1024)
        };
        MemInfo {
            total_mib: field("MemTotal"),
            available_mib: field("MemAvailable"),
            swap_free_mib: field("SwapFree"),
        }
    }

    pub fn read() -> MemInfo {
        std::fs::read_to_string("/proc/meminfo")
            .map_or_else(|_| MemInfo::default(), |t| Self::parse(&t))
    }

    /// What a VM can count on: free RAM, plus zram at the 2:1 it at least compresses to.
    pub fn usable_mib(&self) -> u64 {
        self.available_mib + self.swap_free_mib / 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_counts_half_of_the_swap() {
        let m = MemInfo::parse("MemTotal:       11534336 kB\nMemFree: 1 kB\nMemAvailable:    5242880 kB\nSwapFree:   2097152 kB\n");
        assert_eq!(
            (m.total_mib, m.available_mib, m.swap_free_mib),
            (11264, 5120, 2048)
        );
        assert_eq!(m.usable_mib(), 6144);
    }
}
