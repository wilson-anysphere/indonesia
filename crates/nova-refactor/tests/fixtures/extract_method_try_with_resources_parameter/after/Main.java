import java.io.BufferedReader;
import java.io.StringReader;

class C {
    void m(String s) throws Exception {
        try (BufferedReader br = new BufferedReader(new StringReader(s))) {
            extracted(br);
        }
    }

    private void extracted(BufferedReader br) {
        System.out.println(br.readLine());
    }
}

