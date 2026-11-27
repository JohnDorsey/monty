# `__len__` on user classes, for `len()` and truth tests. Runs on Monty and CPython.


class Sized:
    def __init__(self, result):
        self.result = result

    def __len__(self):
        return self.result


def check_error(operation, value, error_type, message):
    try:
        operation(value)
        assert False, 'expected the operation to raise'
    except error_type as exc:
        assert str(exc) == message


# === The answer ===
assert len(Sized(0)) == 0
assert len(Sized(3)) == 3
assert len(Sized(2**63 - 1)) == 2**63 - 1
assert bool(Sized(0)) is False
assert bool(Sized(3)) is True
assert Sized('raw').__len__() == 'raw', 'a direct call is not checked'


class Index:
    def __index__(self):
        return 5


assert len(Sized(Index())) == 5
assert bool(Sized(Index())) is True
assert type(len(Sized(True))) is int
assert len(Sized(True)) == 1
assert len(Sized(False)) == 0


# === What may not be answered, the same from `len()` and from a truth test ===
rejected = [
    ('raw', TypeError, "'str' object cannot be interpreted as an integer"),
    (1.0, TypeError, "'float' object cannot be interpreted as an integer"),
    (None, TypeError, "'NoneType' object cannot be interpreted as an integer"),
    (-1, ValueError, '__len__() should return >= 0'),
    (-(10**30), ValueError, '__len__() should return >= 0'),
    (2**63, OverflowError, "cannot fit 'int' into an index-sized integer"),
    (10**30, OverflowError, "cannot fit 'int' into an index-sized integer"),
]


# === Class lookup only, inheritance, and changing the class ===
class Plain:
    pass


plain = Plain()
plain.__len__ = lambda: 0
assert bool(plain) is True, 'an instance attribute is not the hook'
check_error(len, plain, TypeError, "object of type 'Plain' has no len()")
Plain.__len__ = lambda self: 0
assert bool(plain) is False
assert len(plain) == 0


class Base:
    def __len__(self):
        return 0


class Inherits(Base):
    pass


class Overrides(Base):
    def __len__(self):
        return 2


class Mixed(Plain, Base):
    pass


assert len(Inherits()) == 0
assert bool(Inherits()) is False
assert len(Overrides()) == 2
assert bool(Overrides()) is True
assert len(Mixed()) == 0


class Unsized:
    __len__ = None


check_error(len, Unsized(), TypeError, "'NoneType' object is not callable")
check_error(bool, Unsized(), TypeError, "'NoneType' object is not callable")


# === `__bool__` is asked first ===
class Both:
    def __bool__(self):
        return True

    def __len__(self):
        raise ValueError('__len__ must not be asked')


assert bool(Both()) is True


class Refuses:
    __bool__ = None

    def __len__(self):
        return 1


check_error(bool, Refuses(), TypeError, "'Refuses' cannot be interpreted as a boolean")
assert len(Refuses()) == 1


# === Truth tests of the language ask `__len__` ===
empty = Sized(0)
full = Sized(1)

assert (not empty) is True
assert (not full) is False
assert (empty or full) is full
assert (full and empty) is empty
assert ('then' if empty else 'else') == 'else'
assert all([full, empty]) is False
assert any([empty, full]) is True
assert list(filter(None, [empty, full])) == [full]
assert [number for number in [0, 1, 2] if Sized(number)] == [1, 2]


class Shrinking:
    def __init__(self):
        self.items = [1, 2, 3]

    def __len__(self):
        return len(self.items)


shrinking = Shrinking()
visits = []
while shrinking:
    visits.append(shrinking.items.pop())
assert visits == [3, 2, 1]


class Container:
    def __contains__(self, item):
        return Sized(item)


assert (0 in Container()) is False
assert (1 in Container()) is True


# === A failing `__len__` propagates ===
class Raising:
    def __len__(self):
        raise ValueError('length failed')


def in_if(value):
    if value:
        pass


sites = [len, bool, in_if, lambda value: not value, lambda value: all([value]), lambda value: list(filter(None, [value]))]
# Repeated so that a reference leaked on an error path would pile up.
for _ in range(5):
    for site in sites:
        check_error(site, Raising(), ValueError, 'length failed')
    for result, error_type, message in rejected:
        check_error(len, Sized(result), error_type, message)
        check_error(bool, Sized(result), error_type, message)
    check_error(len, Sized(['fresh']), TypeError, "'list' object cannot be interpreted as an integer")
    assert len(Sized(Index())) == 5
