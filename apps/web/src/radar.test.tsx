import { render, screen, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { CategoryBalancePanel, deriveCategoryBalance } from './radar';
import { category } from './test/events';

describe('deriveCategoryBalance', () => {
  it('leaves facets out: they re-cut compute rather than adding an axis', () => {
    const balance = deriveCategoryBalance(
      [category('compute', 800), category('single_core', 300, 0), category('memory', 1000)],
      null,
    );

    expect(balance.scoredCategories.map((c) => c.key)).toEqual(['compute', 'memory']);
    expect(balance.weakest?.key).toBe('compute');
    expect(balance.strongest?.key).toBe('memory');
  });

  it('is the weakest category over the geometric mean', () => {
    // Geometric mean of 400, 800 and 1600 is exactly 800.
    const balance = deriveCategoryBalance(
      [category('compute', 400), category('memory', 800), category('storage', 1600)],
      null,
    );

    expect(balance.balanceIndex).toBeCloseTo(0.5, 12);
    expect(balance.weakLinkCap).toBe(1600);
  });

  it('refuses a balance index when any category has no positive score', () => {
    const balance = deriveCategoryBalance([category('compute', 0), category('memory', 800)], null);

    expect(balance.balanceIndex).toBeNull();
  });

  it('sees a total sitting on the weak-link cap, through float noise', () => {
    const categories = [category('compute', 400), category('memory', 2000)];

    expect(deriveCategoryBalance(categories, 1600 * (1 + 1e-12)).totalIsAtTheWeakLinkCap).toBe(true);
    expect(deriveCategoryBalance(categories, 1500).totalIsAtTheWeakLinkCap).toBe(false);
    expect(deriveCategoryBalance(categories, null).totalIsAtTheWeakLinkCap).toBe(false);
  });

  it('has nothing to say before a score arrives', () => {
    const balance = deriveCategoryBalance([], null);

    expect(balance.weakest).toBeNull();
    expect(balance.balanceIndex).toBeNull();
    expect(balance.weakLinkCap).toBeNull();
  });
});

describe('CategoryBalancePanel', () => {
  it('renders nothing before the first score', () => {
    const { container } = render(<CategoryBalancePanel categories={[]} total={null} />);
    expect(container.innerHTML).toBe('');
  });

  it('draws no polygon below three axes, and says why', () => {
    const { container } = render(
      <CategoryBalancePanel categories={[category('compute', 800), category('memory', 900)]} total={null} />,
    );

    expect(container.querySelector('svg')).toBeNull();
    expect(screen.getByText(/No shape drawn/)).toBeTruthy();
  });

  it('draws a decorative radar and repeats every score as ranked text', () => {
    const { container } = render(
      <CategoryBalancePanel
        categories={[category('compute', 900), category('memory', 400), category('storage', 1200)]}
        total={1500}
      />,
    );

    const svg = container.querySelector('svg.radar');
    expect(svg?.getAttribute('aria-hidden')).toBe('true');

    const readout = container.querySelector('.radar-readout dl')!;
    const rows = within(readout as HTMLElement).getAllByRole('term').map((term) => term.textContent);
    expect(rows).toEqual(['Storage', 'Compute', 'Memory']);
    expect(within(readout as HTMLElement).getByText('weakest').closest('div')?.textContent).toContain('Memory');
  });

  it('says so when the total is the weak-link cap', () => {
    render(
      <CategoryBalancePanel
        categories={[category('compute', 400), category('memory', 2000), category('storage', 2000)]}
        total={1600}
      />,
    );

    expect(screen.getByRole('note').textContent).toContain('Weak-link cap applied');
  });

  it('withholds the balance word with a single category', () => {
    render(<CategoryBalancePanel categories={[category('compute', 800)]} total={800} />);

    expect(screen.getByText('1.00')).toBeTruthy();
    expect(screen.queryByText('even')).toBeNull();
    expect(screen.getByText(/1\.00 by construction/)).toBeTruthy();
  });
});
