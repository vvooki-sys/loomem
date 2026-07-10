import { describe, it, expect } from 'vitest';

describe('test runner smoke', () => {
  it('vitest runs and assertions work', () => {
    expect(1 + 1).toBe(2);
  });

  it('jest-dom matchers are loaded', () => {
    const el = document.createElement('div');
    el.textContent = 'hello';
    document.body.appendChild(el);
    expect(el).toBeInTheDocument();
    expect(el).toHaveTextContent('hello');
    document.body.removeChild(el);
  });
});
