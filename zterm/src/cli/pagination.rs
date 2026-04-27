use crate::cli::theme::Theme;
use std::io::{self, Write};

/// Response pagination handler for long outputs
pub struct Paginator {
    lines: Vec<String>,
    page_size: usize,
    current_page: usize,
}

impl Paginator {
    /// Create paginator from response text
    pub fn new(text: &str, page_size: usize) -> Self {
        let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();

        Self {
            lines,
            page_size,
            current_page: 0,
        }
    }

    /// Check if pagination is needed
    pub fn needs_pagination(&self) -> bool {
        self.lines.len() > self.page_size
    }

    /// Get total number of pages
    pub fn total_pages(&self) -> usize {
        self.lines.len().div_ceil(self.page_size)
    }

    /// Get current page lines
    fn current_page_lines(&self) -> Vec<&String> {
        let start = self.current_page * self.page_size;
        let end = (start + self.page_size).min(self.lines.len());

        self.lines[start..end].iter().collect()
    }

    /// Display current page with pagination controls
    pub fn display(&self) {
        println!();
        for line in self.current_page_lines() {
            println!("{}", line);
        }

        // Show pagination controls if needed
        if self.needs_pagination() {
            self.show_pagination_controls();
        }
        println!();
    }

    /// Show pagination footer with navigation hints
    fn show_pagination_controls(&self) {
        let total = self.total_pages();
        let current = self.current_page + 1;

        println!();
        println!(
            "{}{}─{} Page {}/{} {}─{}",
            Theme::CYAN,
            "─".repeat(20),
            Theme::BRIGHT_BLUE,
            current,
            total,
            Theme::CYAN,
            "─".repeat(20)
        );
        println!(
            "{}  {} (n)ext  {} (p)revious  ✕ (q)uit paging{}",
            Theme::BRIGHT_BLUE,
            if self.current_page < total - 1 {
                "→"
            } else {
                " "
            },
            if self.current_page > 0 { "←" } else { " " },
            Theme::RESET
        );
    }

    /// Display and handle pagination interactively
    pub fn display_interactive(&mut self) -> Result<(), std::io::Error> {
        if !self.needs_pagination() {
            // No pagination needed, just display all
            self.display();
            return Ok(());
        }

        // Show first page
        self.display();

        // Interactive pagination
        loop {
            print!(
                "{}[Page {}/{}] (n)ext, (p)revious, (q)uit: {}",
                Theme::BRIGHT_BLUE,
                self.current_page + 1,
                self.total_pages(),
                Theme::RESET
            );
            io::stdout().flush()?;

            let mut response = String::new();
            io::stdin().read_line(&mut response)?;

            match response.trim().to_lowercase().as_str() {
                "n" | "next" => {
                    if self.next_page() {
                        println!();
                        self.display();
                    } else {
                        println!(
                            "{}Already at last page{}",
                            Theme::BRIGHT_YELLOW,
                            Theme::RESET
                        );
                    }
                }
                "p" | "prev" | "previous" => {
                    if self.prev_page() {
                        println!();
                        self.display();
                    } else {
                        println!(
                            "{}Already at first page{}",
                            Theme::BRIGHT_YELLOW,
                            Theme::RESET
                        );
                    }
                }
                "q" | "quit" | "x" | "" => {
                    println!();
                    break;
                }
                _ => {
                    println!(
                        "{}Invalid input. Use 'n', 'p', or 'q'.{}",
                        Theme::BRIGHT_YELLOW,
                        Theme::RESET
                    );
                }
            }
        }

        Ok(())
    }

    /// Go to next page
    fn next_page(&mut self) -> bool {
        if self.current_page < self.total_pages() - 1 {
            self.current_page += 1;
            true
        } else {
            false
        }
    }

    /// Go to previous page
    fn prev_page(&mut self) -> bool {
        if self.current_page > 0 {
            self.current_page -= 1;
            true
        } else {
            false
        }
    }

    /// Reset to first page
    pub fn reset(&mut self) {
        self.current_page = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pagination_no_needed() {
        let text = "short\ntext";
        let paginator = Paginator::new(text, 10);
        assert!(!paginator.needs_pagination());
    }

    #[test]
    fn test_pagination_needed() {
        let text = (0..20)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let paginator = Paginator::new(&text, 5);
        assert!(paginator.needs_pagination());
        assert_eq!(paginator.total_pages(), 4);
    }

    #[test]
    fn test_pagination_navigation() {
        let text = (0..20)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let mut paginator = Paginator::new(&text, 5);
        assert_eq!(paginator.current_page, 0);
        assert!(paginator.next_page());
        assert_eq!(paginator.current_page, 1);
        assert!(paginator.prev_page());
        assert_eq!(paginator.current_page, 0);
    }
}
