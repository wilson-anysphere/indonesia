class TernaryAndLabels{
int m(int x,int y){
return x>0?x:y;
}

int n(int a,int b,int c,int d,int e){
return a?b:c?d:e;
}

void labels(){
outer:for(int i=0;i<1;i++){break outer;}
}
}

