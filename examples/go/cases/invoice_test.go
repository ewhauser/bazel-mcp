package invoice

import "testing"

func TestInvoiceTotal(t *testing.T) {
	got := InvoiceTotal(40)
	want := 42
	if got != want {
		t.Errorf("invoice total mismatch: got %d, want %d", got, want)
	}
}
