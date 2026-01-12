import java.io.BufferedReader;
import java.io.StringReader;

class C {
    void m(String s) throws Exception {
        try (BufferedReader br = new BufferedReader(new StringReader(s))) {
            /*start*/System.out.println(br.readLine());/*end*/
        }
    }
}

