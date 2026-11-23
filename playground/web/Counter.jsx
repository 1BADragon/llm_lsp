// Throwaway code for trying llm_lsp. Edit freely.
import { useEffect, useState } from "react";

export default function Counter({ step }) {
  const [count, setCount] = useState(0);

  useEffect(() => {
    // Deliberate bug: stale closure. `count` is captured once, so the
    // counter sticks at `step`.
    const id = setInterval(() => setCount(count + step), 1000);
    return () => clearInterval(id);
  }, []);

  return <button onClick={() => setCount(0)}>{count}</button>;
}
