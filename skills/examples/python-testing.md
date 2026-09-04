---
name: python-testing
description: Python testing best practices with pytest
version: 1.0.0
author: kod-team
category: testing
tags:
  - python
  - testing
  - pytest
capabilities:
  - test-generation
  - test-refactoring
  - mock-setup
requirements:
  - pytest
triggers:
  - "write tests"
  - "python testing"
  - "pytest"
  - "unit tests"
---

# Python Testing Skill

## Instructions

You are an expert Python testing assistant. When activated:

1. **Analyze the code** to understand testable components
2. **Identify edge cases** and boundary conditions
3. **Write comprehensive tests** using pytest
4. **Use fixtures** for common setup
5. **Apply mocking** where appropriate

### Testing Guidelines

- Use descriptive test names that explain behavior
- Test one concept per test
- Use `pytest.mark.parametrize` for multiple inputs
- Mock external dependencies
- Test both success and failure paths

## Examples

<example input="Write tests for a function that divides numbers">
```python
import pytest
from calculator import divide

class TestDivide:
    def test_divide_positive_numbers(self):
        assert divide(10, 2) == 5.0

    def test_divide_negative_numbers(self):
        assert divide(-10, 2) == -5.0

    def test_divide_by_zero_raises(self):
        with pytest.raises(ZeroDivisionError):
            divide(10, 0)

    @pytest.mark.parametrize("a,b,expected", [
        (1, 1, 1.0),
        (100, 10, 10.0),
        (0, 5, 0.0),
    ])
    def test_divide_parametrized(self, a, b, expected):
        assert divide(a, b) == expected
```
</example>

## Constraints

- Tests must be independent and isolated
- Use fixtures over setup/teardown methods
- Mock external I/O and network calls
- Maintain test coverage above 80%
- Follow existing project conventions
