// Throwaway code for trying llm_lsp. Edit freely.

export interface Item {
  sku: string;
  price: number;
  qty: number;
}

export type Discount = { code: string; percent: number };

export class Cart {
  private items = new Map<string, Item>();

  add(item: Item): void {
    const existing = this.items.get(item.sku);
    if (existing) {
      existing.qty += item.qty;
    } else {
      this.items.set(item.sku, { ...item });
    }
  }

  total(discount?: Discount): number {
    let sum = 0;
    for (const item of this.items.values()) {
      sum += item.price * item.qty;
    }
    return discount ? sum * (1 - discount.percent / 100) : sum;
  }

  async checkout(pay: (amount: number) => Promise<void>): Promise<void> {
    // Deliberate bug: the payment isn't awaited, so failures are lost and
    // the cart is cleared before payment completes.
    pay(this.total());
    this.items.clear();
  }
}

export const formatPrice = (cents: number): string => `$${(cents / 100).toFixed(2)}`;
