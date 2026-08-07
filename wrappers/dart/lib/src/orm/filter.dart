/// Typed filter expressions for the query builder.
///
/// A [Col] is a typed reference to a column; its comparison methods build a
/// [Filter]. Filters compose with `&` (AND) and `|` (OR) and lower to a
/// parameterised SGQL `WHERE` fragment — values become `$1`, `$2`, … bindings,
/// never interpolated, so quoting and types are always correct.
library;

/// Accumulates positional parameters while a filter renders to SGQL.
class SqlContext {
  final List<Object?> params = [];

  /// Register [value] and return its placeholder (`$1`, `$2`, …).
  String placeholder(Object? value) {
    params.add(value);
    return '\$${params.length}';
  }
}

/// A boolean predicate over a row. Compose with `&` / `|`.
abstract class Filter {
  const Filter();

  /// Render to an SGQL boolean expression, binding literals via [ctx].
  String render(SqlContext ctx);

  Filter operator &(Filter other) => AndFilter(this, other);
  Filter operator |(Filter other) => OrFilter(this, other);
}

/// A typed column reference. Comparison methods are type-checked: `price.lt('x')`
/// on a `Col<int>` is a compile error.
class Col<T> {
  final String name;
  const Col(this.name);

  Filter eq(T value) => CompareFilter(name, '=', value);
  Filter neq(T value) => CompareFilter(name, '!=', value);
  Filter lt(T value) => CompareFilter(name, '<', value);
  Filter lte(T value) => CompareFilter(name, '<=', value);
  Filter gt(T value) => CompareFilter(name, '>', value);
  Filter gte(T value) => CompareFilter(name, '>=', value);

  /// `name BETWEEN lo AND hi` (inclusive).
  Filter between(T lo, T hi) => BetweenFilter(name, lo, hi);
}

/// `name OP $n`.
class CompareFilter extends Filter {
  final String column;
  final String op;
  final Object? value;
  const CompareFilter(this.column, this.op, this.value);

  @override
  String render(SqlContext ctx) => '$column $op ${ctx.placeholder(value)}';
}

/// `name BETWEEN $lo AND $hi`.
class BetweenFilter extends Filter {
  final String column;
  final Object? lo;
  final Object? hi;
  const BetweenFilter(this.column, this.lo, this.hi);

  @override
  String render(SqlContext ctx) =>
      '$column BETWEEN ${ctx.placeholder(lo)} AND ${ctx.placeholder(hi)}';
}

/// `(a AND b)`.
class AndFilter extends Filter {
  final Filter a;
  final Filter b;
  const AndFilter(this.a, this.b);

  @override
  String render(SqlContext ctx) => '(${a.render(ctx)} AND ${b.render(ctx)})';
}

/// `(a OR b)`.
class OrFilter extends Filter {
  final Filter a;
  final Filter b;
  const OrFilter(this.a, this.b);

  @override
  String render(SqlContext ctx) => '(${a.render(ctx)} OR ${b.render(ctx)})';
}
