"""A real workload: parse and transpile SQL with sqlglot.

Chosen because it is pure Python and call-dense -- a recursive-descent
parser and an AST visitor are close to the worst case for a tracer that
charges per function call. A workload dominated by C extensions would
flatter trace0 by simply not emitting events.
"""

import sqlglot

QUERIES = [
    """
    SELECT a.customer_id, SUM(o.total) AS lifetime
    FROM customers a
    JOIN orders o ON o.customer_id = a.customer_id
    WHERE o.created_at > '2024-01-01' AND a.region IN ('eu', 'us')
    GROUP BY a.customer_id
    HAVING SUM(o.total) > 1000
    ORDER BY lifetime DESC
    LIMIT 50
    """,
    """
    WITH ranked AS (
      SELECT p.sku, p.price,
             ROW_NUMBER() OVER (PARTITION BY p.category ORDER BY p.price DESC) AS rn
      FROM products p
      WHERE p.active = TRUE
    )
    SELECT sku, price FROM ranked WHERE rn <= 3
    """,
    """
    SELECT CASE WHEN x > 10 THEN 'high' WHEN x > 5 THEN 'mid' ELSE 'low' END AS bucket,
           COUNT(*) AS n, AVG(y) AS mean_y
    FROM measurements
    WHERE ts BETWEEN '2024-06-01' AND '2024-07-01'
    GROUP BY 1
    """,
]

ROUNDS = 400


def main() -> None:
    total = 0
    for _ in range(ROUNDS):
        for query in QUERIES:
            for dialect in ("postgres", "duckdb"):
                total += len(sqlglot.transpile(query, read="mysql", write=dialect))
    print(f"transpiled {total} statements")


if __name__ == "__main__":
    main()
