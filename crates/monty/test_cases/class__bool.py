# `__bool__` on user classes. Runs on Monty and CPython. `__len__`, which a truth test asks after
# `__bool__`, is in `class__len.py`.


class Truth:
    def __init__(self, result):
        self.result = result

    def __bool__(self):
        return self.result


def check_error(operation, value, error_type, message):
    try:
        operation(value)
        assert False, 'expected the operation to raise'
    except error_type as exc:
        assert str(exc) == message


# === The answer, and what may be answered ===
assert bool(Truth(True)) is True
assert bool(Truth(False)) is False
assert Truth('raw').__bool__() == 'raw', 'a direct call is not checked'
for raw, name in [(1, 'int'), (0, 'int'), (None, 'NoneType'), ('', 'str'), ([], 'list'), (Truth(True), 'Truth')]:
    check_error(bool, Truth(raw), TypeError, f'__bool__ should return bool, returned {name}')


# === Class lookup only, inheritance, and changing the class ===
class Plain:
    pass


plain = Plain()
plain.__bool__ = lambda: False
assert bool(plain) is True, 'an instance attribute is not the hook'
Plain.__bool__ = lambda self: False
assert bool(plain) is False


class Base:
    def __bool__(self):
        return False


class Inherits(Base):
    pass


class Overrides(Base):
    def __bool__(self):
        return True


class Mixed(Plain, Base):
    pass


assert bool(Inherits()) is False
assert bool(Overrides()) is True
assert bool(Mixed()) is False, 'Plain.__bool__ was set above and comes first in the MRO'


class NoBool:
    __bool__ = None


check_error(bool, NoBool(), TypeError, "'NoBool' cannot be interpreted as a boolean")


# === Every place that tests truth ===
no = Truth(False)
yes = Truth(True)

assert (not no) is True
assert (not yes) is False
assert (no or yes) is yes, '`or` tests the left operand and returns an operand'
assert (yes and no) is no
assert (no and yes) is no
assert (yes or no) is yes
assert ('then' if yes else 'else') == 'then'
assert ('then' if no else 'else') == 'else'

if no:
    assert False, 'a false instance must not enter `if`'

assert all([yes, no]) is False
assert all([yes, yes]) is True
assert any([no, yes]) is True
assert any([no, no]) is False
assert list(filter(None, [no, yes])) == [yes]
assert list(filter(lambda item: item, [no, yes])) == [yes]
assert list(filter(lambda number: Truth(number > 1), [1, 2, 3])) == [2, 3]
assert [number for number in [0, 1, 2] if Truth(number)  is not None and Truth(number > 0)] == [1, 2]
assert list(item for item in [no, yes] if item) == [yes]


class Countdown:
    def __init__(self):
        self.remaining = 3

    def __bool__(self):
        self.remaining -= 1
        return self.remaining >= 0


counter = Countdown()
visits = []
while counter:
    visits.append(counter.remaining)
assert visits == [2, 1, 0]


class Container:
    def __init__(self, result):
        self.result = result

    def __contains__(self, item):
        return self.result


assert (1 in Container(no)) is False
assert (1 in Container(yes)) is True
assert (1 not in Container(no)) is True


class Equal:
    def __init__(self, result):
        self.result = result

    def __eq__(self, other):
        return self.result


assert (Equal(no) in [1]) is False
assert (Equal(yes) in [1]) is True


class Manager:
    def __init__(self, result):
        self.result = result

    def __enter__(self):
        return self

    def __exit__(self, typ, value, tb):
        return self.result


with Manager(yes):
    raise ValueError('suppressed because __exit__ answered a true instance')

propagated = False
try:
    with Manager(no):
        raise ValueError('not suppressed')
except ValueError:
    propagated = True
assert propagated

match 1:
    case _ if no:
        assert False, 'a false guard must not match'
    case _ if yes:
        pass

assert list(zip([1], [2, 3], strict=no)) == [(1, 2)]
check_error(lambda flag: list(zip([1], [2, 3], strict=flag)), yes, ValueError, 'zip() argument 2 is longer than argument 1')
assert list(map(str, [1], strict=no)) == ['1']

try:
    assert no, 'false instance'
    reached = True
except AssertionError as exc:
    reached = False
    assert str(exc).startswith('false instance'), 'Monty appends what it saw (limitations/assert.md)'
assert reached is False


# === A raising `__bool__` propagates from every site ===
class Raising:
    def __bool__(self):
        raise ValueError('truth failed')


class RaisingResult:
    def __eq__(self, other):
        return Raising()

    def __contains__(self, item):
        return Raising()


def in_if(value):
    if value:
        pass


def in_while(value):
    while value:
        break


def in_comprehension(value):
    return [1 for item in [value] if item]


def in_guard(value):
    match 1:
        case _ if value:
            pass


def in_exit(value):
    with Manager(value):
        raise KeyError('replaced by the error from __bool__')


sites = [
    bool,
    in_if,
    in_while,
    lambda value: not value,
    lambda value: value and 1,
    lambda value: value or 1,
    lambda value: 1 if value else 0,
    lambda value: all([value]),
    lambda value: any([value]),
    lambda value: list(filter(None, [value])),
    lambda value: list(filter(lambda item: item, [value])),
    in_comprehension,
    in_guard,
    in_exit,
    lambda value: list(zip([1], strict=value)),
    lambda value: list(map(str, [1], strict=value)),
]
# Repeated so that a reference leaked on an error path would pile up.
for _ in range(5):
    for site in sites:
        check_error(site, Raising(), ValueError, 'truth failed')
    check_error(lambda value: 1 in value, RaisingResult(), ValueError, 'truth failed')
    check_error(lambda value: value in [1], RaisingResult(), ValueError, 'truth failed')
    check_error(bool, Truth(['fresh']), TypeError, '__bool__ should return bool, returned list')


# === `__bool__` may change what is being walked ===
class Clearing:
    def __init__(self, items, result):
        self.items = items
        self.result = result

    def __bool__(self):
        self.items.clear()
        return self.result


items = []
items.extend([Clearing(items, True), Raising()])
assert all(items) is True
items.extend([Clearing(items, False), Raising()])
assert any(items) is False
items.extend([Clearing(items, True), Raising()])
assert len(list(filter(None, items))) == 1
