%token A B C D E
%%
s: A x D | A y E | B x E | B y D ;
x: C ;
y: C ;
