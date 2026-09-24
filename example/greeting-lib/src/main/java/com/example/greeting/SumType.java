package one.example.com;

public sealed interface SumType{
    public record Type1(int val) implements SumType {}
    public record Type2(String val) implements SumType {}
    public record Type3(float val) implements SumType {}
    public record Type4(double val) implements SumType {}
}
