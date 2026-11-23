"""Throwaway code for trying llm_lsp. Edit freely."""

from dataclasses import dataclass, field


@dataclass
class Item:
    name: str
    price: float
    qty: int = 0


@dataclass
class Inventory:
    items: dict[str, Item] = field(default_factory=dict)

    def add(self, name: str, price: float, qty: int = 1) -> None:
        if name in self.items:
            self.items[name].qty += qty
        else:
            self.items[name] = Item(name, price, qty)

    def remove(self, name: str, qty: int = 1) -> None:
        item = self.items[name]
        # Deliberate bug: qty can go negative.
        item.qty -= qty

    def total_value(self) -> float:
        return sum(i.price * i.qty for i in self.items.values())

    def low_stock(self, threshold: int = 2) -> list[str]:
        return sorted(n for n, i in self.items.items() if i.qty <= threshold)


def main() -> None:
    inv = Inventory()
    inv.add("widget", 2.50, 10)
    inv.add("gadget", 12.00, 1)
    inv.remove("widget", 3)
    print(f"total: {inv.total_value():.2f}")
    print(f"low: {inv.low_stock()}")


if __name__ == "__main__":
    main()
