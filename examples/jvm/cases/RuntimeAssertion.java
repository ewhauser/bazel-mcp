package invoice;

public final class RuntimeAssertion {
  public static void main(String[] args) {
    throw new AssertionError("invoice total mismatch: expected 42 but was 41");
  }
}
