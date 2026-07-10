import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/react';

function Hello({ name }) {
  return <h1>Hello, {name}</h1>;
}

describe('react-testing-library smoke', () => {
  it('renders a component and queries DOM', () => {
    render(<Hello name="Loomem" />);
    expect(screen.getByRole('heading')).toHaveTextContent('Hello, Loomem');
  });
});
