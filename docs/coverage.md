# Code Coverage

Simulife-rs uses [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) for source-based code coverage reporting.

## Quick Start

### From Neovim

```vim
:CoverageLlvmCov
```

This generates an HTML coverage report and opens it in your browser. Keybind: `<leader>tc`

### From Terminal

```bash
# Generate HTML report
cargo llvm-cov --html

# Open the report (macOS)
open target/llvm-cov/html/index.html

# Other formats
cargo llvm-cov --json                    # JSON output
cargo llvm-cov --lcov                    # LCOV format (for CI/tools)
cargo llvm-cov --cobertura               # Cobertura XML format
```

## Output

- **HTML Report**: `target/llvm-cov/html/index.html` — interactive, file-by-file breakdown
- **JSON**: stdout — parseable for automation
- **LCOV**: stdout — standard format for CI/CD integration

## Coverage Metrics

The report shows:
- **Line coverage**: % of lines executed by tests
- **Region coverage**: % of branch conditions taken
- **Function coverage**: % of functions called

Green = well-tested, Yellow = partial coverage, Red = untested.

## Tips

1. **Run tests first**: Coverage only measures lines executed by tests. Add more tests to increase coverage.

2. **Baseline workflow**:
   ```bash
   cargo test                  # Run tests
   cargo llvm-cov --html       # Generate coverage
   open target/llvm-cov/html/index.html
   ```

3. **CI integration**: Use `--lcov` output with services like Codecov:
   ```bash
   cargo llvm-cov --lcov --output-path coverage.lcov
   ```

4. **Specific package**: 
   ```bash
   cargo llvm-cov -p server --html
   cargo llvm-cov -p viewer --html
   ```

5. **Excluding paths**:
   ```bash
   cargo llvm-cov --html --exclude-files 'tests/*' 'examples/*'
   ```

## Neovim Integration Details

The `:CoverageLlvmCov` command:
- Runs `cargo llvm-cov --html` in your project directory
- Saves report to `target/llvm-cov/html/`
- Automatically opens the HTML report in your default browser (macOS: `open`, Linux: `xdg-open`)

**Keybind**: `<leader>tc` → `[T]est [C]overage`

## Current Coverage

Run `:CoverageLlvmCov` to generate a fresh report and see where tests are concentrated.
